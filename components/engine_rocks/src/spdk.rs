// Copyright 2021 TiKV Project Authors. Licensed under Apache-2.0.

use std::sync::Arc;

use crate::raw::Env;

pub fn get_env(
    fsname: &str,
    confname: &str,
    bdevname: &str,
    cache_size_in_mb: u64,
) -> Arc<Env> {
    // TODO: handle the error
    Arc::new(Env::new_spdk(fsname, confname, bdevname, cache_size_in_mb))
}