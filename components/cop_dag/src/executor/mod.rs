// Copyright 2017 TiKV Project Authors. Licensed under Apache-2.0.

use std::sync::Arc;

use cop_datatype::prelude::*;
use cop_datatype::FieldTypeFlag;
use kvproto::coprocessor::KeyRange;
use tipb::expression::{Expr, ExprType};
use tipb::schema::ColumnInfo;

use tikv_util::codec::number;
use tikv_util::collections::HashSet;

use crate::codec::datum::{self, Datum, DatumEncoder};
use crate::codec::table::{self, RowColsDict};
use crate::expr::{EvalContext, EvalWarnings};
use crate::util;
use crate::*;

mod aggregate;
mod aggregation;
mod index_scan;
mod limit;
mod scan;
mod selection;
mod table_scan;
mod topn;
mod topn_heap;

mod metrics;

pub use self::aggregation::{HashAggExecutor, StreamAggExecutor};
pub use self::index_scan::IndexScanExecutor;
pub use self::limit::LimitExecutor;
pub use self::metrics::*;
pub use self::scan::ScanExecutor;
pub use self::selection::SelectionExecutor;
pub use self::table_scan::TableScanExecutor;
pub use self::topn::TopNExecutor;

/// An expression tree visitor that extracts all column offsets in the tree.
pub struct ExprColumnRefVisitor {
    cols_offset: HashSet<usize>,
    cols_len: usize,
}

impl ExprColumnRefVisitor {
    pub fn new(cols_len: usize) -> ExprColumnRefVisitor {
        ExprColumnRefVisitor {
            cols_offset: HashSet::default(),
            cols_len,
        }
    }

    pub fn visit(&mut self, expr: &Expr) -> Result<()> {
        if expr.get_tp() == ExprType::ColumnRef {
            let offset = box_try!(number::decode_i64(&mut expr.get_val())) as usize;
            if offset >= self.cols_len {
                return Err(Error::Other(box_err!(
                    "offset {} overflow, should be less than {}",
                    offset,
                    self.cols_len
                )));
            }
            self.cols_offset.insert(offset);
        } else {
            for sub_expr in expr.get_children() {
                self.visit(sub_expr)?;
            }
        }
        Ok(())
    }

    pub fn batch_visit(&mut self, exprs: &[Expr]) -> Result<()> {
        for expr in exprs {
            self.visit(expr)?;
        }
        Ok(())
    }

    pub fn column_offsets(self) -> Vec<usize> {
        self.cols_offset.into_iter().collect()
    }
}

#[derive(Debug)]
pub struct OriginCols {
    pub handle: i64,
    pub data: RowColsDict,
    cols: Arc<Vec<ColumnInfo>>,
}

/// Row generated by aggregation.
#[derive(Debug)]
pub struct AggCols {
    // row's suffix, may be the binary of the group by key.
    suffix: Vec<u8>,
    pub value: Vec<Datum>, // it's public for tests
}

impl AggCols {
    pub fn get_binary(&self) -> Result<Vec<u8>> {
        let mut value =
            Vec::with_capacity(self.suffix.len() + datum::approximate_size(&self.value, false));
        box_try!(value.encode(&self.value, false));
        if !self.suffix.is_empty() {
            value.extend_from_slice(&self.suffix);
        }
        Ok(value)
    }
}

#[derive(Debug)]
pub enum Row {
    Origin(OriginCols),
    Agg(AggCols),
}

impl Row {
    pub fn origin(handle: i64, data: RowColsDict, cols: Arc<Vec<ColumnInfo>>) -> Row {
        Row::Origin(OriginCols::new(handle, data, cols))
    }

    pub fn agg(value: Vec<Datum>, suffix: Vec<u8>) -> Row {
        Row::Agg(AggCols { suffix, value })
    }

    pub fn take_origin(self) -> OriginCols {
        match self {
            Row::Origin(row) => row,
            _ => unreachable!(),
        }
    }

    pub fn get_binary(&self, output_offsets: &[u32]) -> Result<Vec<u8>> {
        match self {
            Row::Origin(row) => row.get_binary(output_offsets),
            Row::Agg(row) => row.get_binary(), // ignore output offsets for aggregation.
        }
    }
}

impl OriginCols {
    pub fn new(handle: i64, data: RowColsDict, cols: Arc<Vec<ColumnInfo>>) -> OriginCols {
        OriginCols { handle, data, cols }
    }

    // get binary of each column in order of columns
    pub fn get_binary_cols(&self) -> Result<Vec<Vec<u8>>> {
        let mut res = Vec::with_capacity(self.cols.len());
        for col in self.cols.iter() {
            if col.get_pk_handle() {
                let v = util::get_pk(col, self.handle);
                let bt = box_try!(datum::encode_value(&[v]));
                res.push(bt);
                continue;
            }
            let col_id = col.get_column_id();
            let value = match self.data.get(col_id) {
                None if col.has_default_val() => col.get_default_val().to_vec(),
                None if col.flag().contains(FieldTypeFlag::NOT_NULL) => {
                    return Err(box_err!("column {} of {} is missing", col_id, self.handle));
                }
                None => box_try!(datum::encode_value(&[Datum::Null])),
                Some(bs) => bs.to_vec(),
            };
            res.push(value);
        }
        Ok(res)
    }

    pub fn get_binary(&self, output_offsets: &[u32]) -> Result<Vec<u8>> {
        // TODO capacity is not enough
        let mut values = Vec::with_capacity(self.data.value.len());
        for offset in output_offsets {
            let col = &self.cols[*offset as usize];
            let col_id = col.get_column_id();
            match self.data.get(col_id) {
                Some(value) => values.extend_from_slice(value),
                None if col.get_pk_handle() => {
                    let pk = util::get_pk(col, self.handle);
                    box_try!(values.encode(&[pk], false));
                }
                None if col.has_default_val() => {
                    values.extend_from_slice(col.get_default_val());
                }
                None if col.flag().contains(FieldTypeFlag::NOT_NULL) => {
                    return Err(box_err!("column {} of {} is missing", col_id, self.handle));
                }
                None => {
                    box_try!(values.encode(&[Datum::Null], false));
                }
            }
        }
        Ok(values)
    }

    // inflate with the real value(Datum) for each columns in offsets
    // inflate with Datum::Null for those cols not in offsets.
    // It's used in expression since column is marked with offset
    // in expression.
    pub fn inflate_cols_with_offsets(
        &self,
        ctx: &EvalContext,
        offsets: &[usize],
    ) -> Result<Vec<Datum>> {
        let mut res = vec![Datum::Null; self.cols.len()];
        for offset in offsets {
            let col = &self.cols[*offset];
            if col.get_pk_handle() {
                let v = util::get_pk(col, self.handle);
                res[*offset] = v;
            } else {
                let col_id = col.get_column_id();
                let value = match self.data.get(col_id) {
                    None if col.has_default_val() => {
                        // TODO: optimize it to decode default value only once.
                        box_try!(table::decode_col_value(
                            &mut col.get_default_val(),
                            ctx,
                            col
                        ))
                    }
                    None if col.flag().contains(FieldTypeFlag::NOT_NULL) => {
                        return Err(box_err!("column {} of {} is missing", col_id, self.handle));
                    }
                    None => Datum::Null,
                    Some(mut bs) => box_try!(table::decode_col_value(&mut bs, ctx, col)),
                };
                res[*offset] = value;
            }
        }
        Ok(res)
    }
}

pub trait Executor {
    fn next(&mut self) -> Result<Option<Row>>;
    fn collect_output_counts(&mut self, counts: &mut Vec<i64>);
    fn collect_metrics_into(&mut self, metrics: &mut ExecutorMetrics);
    fn get_len_of_columns(&self) -> usize;

    /// Only executors with eval computation need to implement `take_eval_warnings`
    /// It returns warnings happened during eval computation.
    fn take_eval_warnings(&mut self) -> Option<EvalWarnings> {
        None
    }

    /// Only `TableScan` and `IndexScan` need to implement `start_scan`.
    fn start_scan(&mut self) {}

    /// Only `TableScan` and `IndexScan` need to implement `stop_scan`.
    ///
    /// It returns a `KeyRange` the executor has scaned.
    fn stop_scan(&mut self) -> Option<KeyRange> {
        None
    }
}

#[cfg(test)]
pub mod tests {
    use std::collections::btree_map::BTreeMap;
    use std::ops::Bound;

    use crate::codec::{table, Datum};
    use crate::executor::{Executor, TableScanExecutor};
    use crate::storage::{Key, KvPair, Scanner, Statistics, Store, Value};
    use crate::{Error, Result};

    use cop_datatype::{FieldTypeAccessor, FieldTypeTp};
    use kvproto::coprocessor::KeyRange;
    use protobuf::RepeatedField;
    use tikv_util::codec::number::NumberEncoder;
    use tipb::{
        executor::TableScan,
        expression::{Expr, ExprType},
        schema::ColumnInfo,
    };

    pub fn build_expr(tp: ExprType, id: Option<i64>, child: Option<Expr>) -> Expr {
        let mut expr = Expr::new();
        expr.set_tp(tp);
        if tp == ExprType::ColumnRef {
            expr.mut_val().encode_i64(id.unwrap()).unwrap();
        } else {
            expr.mut_children().push(child.unwrap());
        }
        expr
    }

    pub fn new_col_info(cid: i64, tp: FieldTypeTp) -> ColumnInfo {
        let mut col_info = ColumnInfo::new();
        col_info.as_mut_accessor().set_tp(tp);
        col_info.set_column_id(cid);
        col_info
    }

    // the first column should be i64 since it will be used as row handle
    pub fn gen_table_data(
        tid: i64,
        cis: &[ColumnInfo],
        rows: &[Vec<Datum>],
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut kv_data = Vec::new();
        let col_ids: Vec<i64> = cis.iter().map(|c| c.get_column_id()).collect();
        for cols in rows.iter() {
            let col_values: Vec<_> = cols.to_vec();
            let value = table::encode_row(col_values, &col_ids).unwrap();
            let key = table::encode_row_key(tid, cols[0].i64());
            kv_data.push((key, value));
        }
        kv_data
    }

    pub struct TestStore {
        storage: BTreeMap<Key, Value>,
    }

    pub struct TestScanner {
        data: std::vec::IntoIter<(Key, Value)>,
    }

    impl TestStore {
        pub fn new(kv_data: &[(Vec<u8>, Vec<u8>)]) -> TestStore {
            TestStore {
                storage: kv_data
                    .iter()
                    .map(|(key, val)| (Key::from_raw(key), val.clone()))
                    .collect(),
            }
        }

        pub fn get_snapshot(&self) -> TestStore {
            TestStore {
                storage: self.storage.clone(),
            }
        }
    }

    impl Store for TestStore {
        type Error = Error;
        type Scanner = TestScanner;

        fn get(&self, key: &Key, _statistics: &mut Statistics) -> Result<Option<Vec<u8>>> {
            Ok(self.storage.get(key).cloned())
        }

        fn batch_get(
            &self,
            keys: &[Key],
            statistics: &mut Statistics,
        ) -> Vec<Result<Option<Vec<u8>>>> {
            keys.iter().map(|key| self.get(key, statistics)).collect()
        }

        fn scanner(
            &self,
            desc: bool,
            key_only: bool,
            lower_bound: Option<Key>,
            upper_bound: Option<Key>,
        ) -> Result<Self::Scanner> {
            let lower = lower_bound
                .as_ref()
                .map_or(Bound::Unbounded, |v| Bound::Included(v));
            let upper = upper_bound
                .as_ref()
                .map_or(Bound::Unbounded, |v| Bound::Excluded(v));

            let mut vec: Vec<(Key, Value)> = self
                .storage
                .range((lower, upper))
                .map(|(k, v)| {
                    let owned_k = k.clone();
                    let owned_v = if key_only { vec![] } else { v.clone() };
                    (owned_k, owned_v)
                })
                .collect();

            if desc {
                vec.reverse();
            }
            Ok(Self::Scanner {
                data: vec.into_iter(),
            })
        }
    }

    impl Scanner for TestScanner {
        type Error = Error;

        fn next(&mut self) -> Result<Option<(Key, Vec<u8>)>> {
            let value = self.data.next();
            match value {
                None => Ok(None),
                Some((k, v)) => Ok(Some((k, v))),
            }
        }

        fn scan(&mut self, limit: usize) -> Result<Vec<Result<KvPair>>> {
            let mut results = Vec::with_capacity(limit);
            while results.len() < limit {
                match self.next() {
                    Ok(Some((k, v))) => {
                        results.push(Ok((k.to_raw().unwrap(), v)));
                    }
                    Ok(None) => break,
                    Err(e) => return Err(e),
                }
            }
            Ok(results)
        }

        fn take_statistics(&mut self) -> Statistics {
            Statistics::default()
        }
    }

    #[inline]
    pub fn get_range(table_id: i64, start: i64, end: i64) -> KeyRange {
        let mut key_range = KeyRange::new();
        key_range.set_start(table::encode_row_key(table_id, start));
        key_range.set_end(table::encode_row_key(table_id, end));
        key_range
    }

    pub fn gen_table_scan_executor(
        tid: i64,
        cis: Vec<ColumnInfo>,
        raw_data: &[Vec<Datum>],
        key_ranges: Option<Vec<KeyRange>>,
    ) -> Box<dyn Executor + Send> {
        let table_data = gen_table_data(tid, &cis, raw_data);
        let test_store = TestStore::new(&table_data);

        let mut table_scan = TableScan::new();
        table_scan.set_table_id(tid);
        table_scan.set_columns(RepeatedField::from_vec(cis.clone()));

        let key_ranges = key_ranges.unwrap_or_else(|| vec![get_range(tid, 0, i64::max_value())]);
        Box::new(TableScanExecutor::table_scan(table_scan, key_ranges, test_store, true).unwrap())
    }
}
