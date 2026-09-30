# TiDB / TiKV 基础语义与 expression evaluator 去重 Demo

本文是唯一主 ExecPlan，供不掌握之前聊天记录的人或 agent 继续实施。持续维护 `Progress`、`Surprises & Discoveries`、`Decision Log` 和 `Outcomes & Retrospective`。格式参考已完整阅读的 TiDB `PLANS.md`；实施前必须重新阅读新 worktree 内的 `AGENTS.md`、`PLANS.md` 及适用的子目录规则。

记录时间：2026-09-28 UTC。文档落盘阶段仅保存、审阅并交付本计划，没有运行迁移。随后用户明确要求“请按照当前计划实施”，并批准 M0–M6 实施计划；**当前已授权创建两个新 worktree、修改产品代码、构建、测试与验收**，用户随后明确要求每完成一步连同 Plan 推送，并指定两仓库为 YangKeao/tidb、YangKeao/tikv；现在授权主 agent 对经验证步骤 commit/push 到统一 expression-unification-demo 分支，不自动开 PR/强推。实际完成状态以 Progress、活动台账及命令证据为准，不能把计划命令当成已执行结果。

唯一可编辑主文件固定为 `/home/agent/tidb/EXPRESSION_UNIFICATION_PLAN.md`，同时覆盖两个仓库。依用户新增的每步双仓推送要求，两仓根目录发布自动生成、逐字一致的Plan快照；它们不是独立编辑源，每次发布从本主文件重新生成并校验同一SHA，不能各自维护。未来若迁移主文件位置，必须明确记录唯一的新位置，避免计划漂移。

## Purpose / Big Picture

用户希望 Rust TiDB、TiKV，未来还有 TiFlash，共用表达式相关实现；这次先完成 TiDB 与 TiKV 的去重 Demo。推进顺序是先 collation（字符串比较、排序键和模式匹配规则），再类型和值语义，再表达式求值。collation 和类型共用是地基，**大多数表达式只执行 TiKV evaluator 并删除 TiDB 对应 native 实现，才是最终目标**。

这里的 kernel 指具体的值运算，例如整数加法、Decimal 舍入、LIKE 字符匹配；evaluator 指决定何时、以哪些行和参数调用这些运算的求值运行时；lowering 指把 TiDB 已推导好类型的表达式转成 TiKV 可以编译执行的构建描述。native 在本文专指现有 Rust TiDB 自己实现的求值代码，不指 Go TiDB，也不意味着 TiKV 通过远程服务运行。

本 Demo 在同一进程内调用 TiKV Rust crates，不需要启动 TiKV 服务或通过存储 RPC 求值。TiDB 保留 SQL 前端、类型推导、schema 元信息、session 和执行器所有权；除明确登记的少量兼容例外外，实际表达式计算只走 TiKV。SQL 的值、类型、NULL、warning、错误和必要的求值顺序保持兼容。

用户明确要求：优先消除重复实现；可以为必要接口、能力和去重修改 TiKV，但避免无关删改；不能为了“少改 TiKV”在 TiDB 留第二份算法；不拆 PR，这是一次连续试验；最终结构和改动必须容易 review；兼容成本过高的少量部分写入文档暂缓；使用多个 subagent 同时工作以加速 Demo。

本计划把“大部分”落实为一个提议并采用的验收目标：至少 90% 当前 Rust TiDB 实际已实现的、归并别名后的非宿主专有标量函数族完成 TiKV-only 迁移。该百分比是目标，不是调查结果。需要在改代码前冻结分母，按 overload、类型/collation/context 域和入口分别记录；函数族现有支持域全部迁移才计为完整迁移，部分迁移单列。不得在遇到困难后缩小分母，不得靠别名、常量或 benchmark 高频调用夸大比例。

算术、常见 CAST、比较、逻辑/条件、普通字符串和数学函数属于主迁移范围。普通 JSON 和日期时间能力也要推进，不能因为少数复杂边界就整族默认搁置。宿主原语、原本未实现的注册项、兼容例外分别完整展示。聚合/window 的参数表达式及共用算术纳入，但不要求把 TiDB 聚合状态机、window executor、planner 整体替换为 TiKV；不能据此声称这些完整执行器已经统一。

## Progress

- [x] (2026-09-28) 用户确认 TiDB 使用官方 `hparser-integration`，TiKV 使用官方 `master`。
- [x] (2026-09-28) 完成只读源码调查，识别基础语义、AST/SQL/PB/vector/unistore 求值入口及主要兼容风险。
- [x] (2026-09-28) 确定唯一实现、TiKV 必要改动、集中适配、显式例外和多 subagent 并行的执行原则。
- [x] (2026-09-28) 将自包含计划写入本文件；本轮不启动产品代码实施。
- [x] (2026-09-28) 完成本文件全文自检、两名 subagent 独立审阅和审阅状态修订；审阅未发现实质遗漏或矛盾。
- [x] (2026-09-28) 用户启动并批准 M0–M6 实施；只读检查目标无冲突、旧实验工作树干净，审阅 TiDB 两提交增量并批准新固定 SHA。
- [x] (2026-09-28 06:03Z) fetch 并创建两个固定 SHA worktree；HEAD/branch/status 已核对，无旧实验修改继承。
- [x] (2026-09-28) 核对两侧 pinned compiler（TiDB rustc 1.100.0-nightly、TiKV 1.95.0-nightly），两侧 `cargo metadata --locked --no-deps` 通过；只读使用旧目录的已安装编译器，Cargo cache/target/logs 单独放新实验。
- [x] (2026-09-28) `make bazel_prepare` 和两侧 locked fetch 通过；保留一个必要生成 BUILD.bazel。
- [x] (2026-09-28) TiDB 原基线 collation 25/25、Go key fixture 1/1 通过；expr lib 1226 pass / 4 fail / 93 ignored，七个 PB typed tests 均通过。四个失败均单独复现，登记为原基线而非迁移引入。
- [x] (2026-09-28) TiKV 原基线 collation 8/8、RPN expr 428/428、Decimal 26/26 通过；运行结束时 TiKV status/diff 干净。原 CMake/Abseil 环境失败使用独立 helper 的 CMAKE policy、GCC14、`-include cstdint` 解决，无 vendor patch。
- [x] (2026-09-28) TiDB 两个 crate 接入相邻 TiKV path dependency，caller root 补7项必要 crate patches；通过 Cargo 保留原 lock 并精确 pin kvproto/tipb/yatp/raft/protobuf/fs2/cmake/sysinfo 到 TiKV baseline commit，最终 `cargo metadata --locked` 通过（job bash-31）。TiKV manifest/lock 未改。
- [x] (2026-09-28) M0 caller 实际编译与初始运行 gate 通过：TiDB pinned compiler 上 bridge11/11、collation25/25、公开契约9/9、原Go fixture1/1、expr LIKE11/11；不是仅 metadata 成功。两侧原基线与静态分母均已审阅；后续完整迁移/兼容验收仍未完成。
- [x] (2026-09-28) 主 agent 审阅并冻结 `evidence/coverage-baseline.json` / `coverage-baseline-notes.md` 的 M0-static-v1：245 个已实现纯/上下文函数族，>=90% 至少221个完整；35宿主族、1 always-NULL stub、12 registry-only/syntax 行单列。生成器只读取固定 Git SHA 对象；所有 family 当前均未迁移。类型域 cross-product/逐项行为仍须验证，静态导航链接不是覆盖证明。
- [x] (2026-09-28) 第一集成源码切片通过：TiKV collation17/17、Decimal33/33、codegen20/20、RPN438/438（原428+local10）；TiDB stringutil13/13、LIKE cache1/1、JSON ops18/18。四个LIKE回归保持原先期望并全部RED→GREEN。只接受已实现域，非M2/M3整体完成。
- [x] (2026-09-28) 独立执行 pinned Go Decimal oracle4例，确认 `0 * -1.1 => 0.0` 和 `0.000 * -1 => 0.000`（含交换顺序）；据此批准仅一条旧 TiKV test_mul 期望 `0`→`0.0` 的语义修正。隐藏storage/result scale与Truncated-zero差异仍未统一。
- [x] (2026-09-28) caller flate2 降级后的 crypto/compression 源测试16 pass /3原有ignored，exit0；不把ignored算作通过，性能和完整codec消费者仍待验证。
- [x] (2026-09-28) 完成首批 General/UCA/LIKE 共享与对应native删除检查点：经生成器删除五份无引用镜像共2,128,092 bytes，保留GB图与HEAD一致；主agent独立post-prune源校验/两仓diff检查通过。datatype/codec/executor/planner/session/partition/stats/unistore 的61项消费者测试全部通过。GB key/compare、charset和两个安全域byte matcher残留逐项登记，不宣称仓库全局无重复；全仓lint与性能仍归M6。
- [x] (2026-09-28) B2.0安全前置真实RED→GREEN：有效u64 marker原0/default7已复现；Default-safe拥有型NULL、Decimal/Duration有效Default及物理NULL字节测试集成后，TiKV datatype全套324/324（无ignored/filtered）通过。还没有wide Decimal数值执行声明。
- [x] (2026-09-28) B2.1 SmallVec依赖已准备：native/caller各增一条锁边，完整TOML排除该单边后SHA256与前值相同，所有版本/source/checksum及其他依赖均不变；两侧正确cwd的offline metadata与locked复核通过。
- [x] (2026-09-28) B2.1 拥有型 Decimal 检查点通过：native datatype330/330（Decimal40）、codegen20/20、aggr40/40；caller 原始值桥11、collation25+9+Go1通过。独立物理40字节适配保留空header、非规范partial shape及全部u8结果scale，逻辑Parts保持严格；raw-like-Go异常域仍单列。编译新增发现compat_v1/算术/unary/时间/UUID复制点，只有机械拥有权修复；至少19个外部生产Copy依赖/10文件，旧15点清单不是穷尽证明。
- [x] (2026-09-28) C2a 实际完整 RPN462/462通过（原438+新增24，无ignored/filtered），严格Int控制、按需输入、资源边界、迭代metadata/drop已有执行证据。普通调用仍eager，未宣称一般SQL NULL-stop或Host生命周期完成。
- [x] (2026-09-28) D1 显式私有接入seed15/15通过；保留真实SQL/PB元信息、schema/physical occurrence和worker-local RPN。TiDB expression全套1245 pass /4同原有失败 /93原有ignored，未出现新增失败。仅核对该显式seed，不激活全局入口，不增加完整族计数。
- [x] (2026-09-28) B2.2-A private add/sub/mul/比较/计数限额检查点：native datatype335/335（新增5项）通过；独立legacy状态测试1项通过。公共Grow导入仍关闭，div/AVG/round/parser/formatter继续实现。主agent独立Go与已验收B2.1 rlib probe以及Fraction验证记录了Overflow格式域与Fixed MOD差异，未重录数值oracle。
- [x] (2026-09-28) B2.2-B private division/integer-pair/AVG检查点：native datatype341/341（新增6）通过，复用原长除法循环，含独立498位完整商与存储值对照；Fixed旧状态/数值oracle未改。新wide Fixed MOD尾部分数header下溢已有受检Err证据，尚未批准该域修正，公共Grow保持关闭。
- [x] (2026-09-28) B2.2-C private round/streaming formatter检查点：datatype347/347、RPN520、aggr40通过。保留原数值/字节oracle；受批Default Display分界实际观察到private legacy=0、default/full=-0.00且Overflow字段不变。u32::MAX结果scale拒绝sink测试无巨量分配；wide Fixed遗留分支仍封闭。
- [x] (2026-09-28) 主agent执行固定Go解析观察18例（无panic）：确认溢出mantissa的exponent-junk可改变状态和shift、词法指数溢出会清sign、VT/FF拒绝，以及正负阈值不对称。零在-1073741824仍OK、-1073741825才Truncated；没有运行旧Rust DB巨量分配路径。随后另加同值9e-82的两种mantissa拼法，Go共20例：长mantissa产生wrapped Overflow和partial 1e72，短式Truncated零；不能把全部Overflow统一饱和。旧18例日志保留。这些是Go政策观察，非共享parser已通过。
- [x] (2026-09-28) B2.2-D private scanner/packing/shift政策检查点：353/353通过（首轮352/1只因新VT假设错误，先用旧编译产物证实VT拒绝/FF接受后仅修新测试，生产与旧fixture不动）；C3d RPN537/aggr40同时通过。20个Go观察已进入选定原生政策测试，公开wide仍关闭。
- [x] (2026-09-28) E发现的PB-origin共享elems缺陷已真实RED→GREEN：原保留provenance变changed、重lower失败；仅私有化effective_type、比较谓词/test-only深拷贝getter及两处测试取值API，保持全部fixture/断言不变。回归1项与完整seed16项通过。
- [x] (2026-09-28) C2b同一driver上的staged HostCall/Fresh-Reuse/cleanup：native RPN499/499（新增37）及aggr40通过，caller seed16通过；父agent复核9文件manifest/hash与核心生命周期源码。未接入实际SQL宿主，也未取得普通调用NULL/profile或完整context/batch证明。
- [x] (2026-09-28) D2-min四文件StructuralOnly边界8/8通过，公共SqlBuild保持；首次编译修复BETWEEN函数别名的两个漏传purpose。完整TiDB expression1254 pass/4同名同panic原失败/93ignored，无新增失败；对应1351个发现测试。准备型CAST仍不可执行。
- [x] (2026-09-28) 独立Fraction/int数值oracle24对/120值已生成并由主agent复核--check及SHA；仅数值、非SQL状态/元信息/AVG oracle，还未算作核心对照测试通过。
- [x] (2026-09-28) B2.2-E private严格words导入和整数转换：scale91的Ok0→应Truncated0真实RED后仅改四个actual-extent循环，GREEN1/full358、RPN537/aggr40、caller1274/4原失败/93ignored。保留元信息/全部已初始化inactive词，metadata-only最大scale不密集分配。独立Go/已验收KV证实旧bounded DOT81乘法确会截去整数低word，不能改成只裁fraction。
- [x] (2026-09-28) B2.2-F MOD实际RED2→GREEN3（含经检查路径后添加的max-u32 metadata-only zero）：仅full-gap预选/copy/header与新visible>255零shape修正，MUL/ROUND/capped-Q/旧numeric-byte oracle保持。与C3b helpers合并native datatype368、RPN537/aggr40通过；不扩大原生raw异常域等价声明。
- [x] (2026-09-28) B2.2-G target/codec实际RED2→GREEN2，datatype374通过：共享checked MAX、convert target前置检查与完整raw-cell fallible copy、encoder写前guard/有界shape日志、同一storage emitter计数后fallible reserve再Rust f64 parse。codec1条/float15条before-after观察完全相同；无新SQL65/30限制、Inf拒绝、Go carry后验或public export。
- [x] (2026-09-28) B2.2-H 外部producer两文件实际RED2→GREEN2；datatype378/RPN575/aggr40通过。新fraction81观察夹具曾错误unwrap旧parser的Truncated，修正为private exact words后才采集before；30 policy+7 private rows逐行before/after相同。共享checked declared target/MAX/borrowed Fixed round，保留UNSPEC bypass、错误/警告及unsigned-last次序；无post-carry新检查。
- [x] (round4，B2.2-I联合验收) datatype381/RPN608/aggr40与caller1292+4原完整failure blocks/93ignored；同源同TEST-profile独立分配探针有效RED→GREEN：8轮MOD/DIV/DAY现在均actual1对baseline1，去掉原额外2/2/1请求，非zero-allocation/general bound。首次DEV build后误用旧TEST rlib的stale RED完整保留，核对SHA并刷新匹配test图后才计GREEN。仅lazy Res factory同一disposition、MOD/DIV闭包及simple-interval延迟文本；无Display/真实overflow消息/产品allocator变化。公开wide仍未放行：真实超大诊断、temporal、Datum/存储字符串/contextual fallible桥继续闭合，DB facade/重复删除未完成。
- [x] (round5，B2.2-J联合验收) Stage1两项真实generic wrapper dispatch RED后，保持4 fallback+18 STORAGE行各9ctx的before序列；仅移除新adapter的2cfg门、改2generic body并加Decimal→G helper，GREEN2/full385。父agent逐行比较22行before/after完全相同，所有旧代码/测试外的变更范围有反向SHA证明。此RED不是实际Decimal ENOMEM，不涉及RESULT Display/temporal/publicwide；post-J/C4实际RPN636正常+1隔离来源、aggr40、caller1310/4原完整失败/93ignored通过比较，另caller datatype11+8通过。
- [x] (round6，B2.2-K RESULT consumer) 真实Datum12.34小malloc8故障同源RED→GREEN：旧SIGABRT+HIT且无RETURNED；新outer InvalidDataType1105+RETURNED/FINISH，owner未变，四路controls通过。仅datum.rs private fallible fmt::Write sink复用未改Display一次，非STORAGE/SQL数值status/普遍OOM恢复。2新test先采集36 bounded raw/result行，after逐字相同；fullDT387/aggr40/RPN636、caller1310/4原完整失败/93ignored。Probe源码e63c85fe…ab04b不改，TEST库e806…→6ff3848d…45047；A probe及K产品/helper/完整receipt两轮独立只读均无域内新增阻塞。Caller首命令误cwd未运行测试，正确cwd重跑后才计通过。
- [x] (2026-09-28) C3a七文件验收：精确PlusInt203的SignedLongLong/Typed Int-or-NULL、Identity、TypedRow/PbRow profile，用同一prepared helper/官方Ordinary帧。native RPN520/520（新增21）、aggr40、caller16+8及全量1254/4同名同panic/93ignored通过相应门槛。保留旧222/control/Host；PB标签一致性不等于摄入证明，AST/batch/其他签名/混合carrier仍拒绝，尚无SQL诊断适配或公共激活。
- [x] (2026-09-28) D3六文件runtime-only caller实际10/10通过：同质Typed、与可信原始wire配对的PB、按需原生kind检查及各computed节点独立元信息。旧D1=16/D2=8，全量1361发现/1264pass/4同名同panic/93ignored；不重解码/重求值、不保留整棵wire、不改默认入口。E确认只读域内无发现；harness关闭失败不作为测试证据。
- [x] (2026-09-28) C3d五文件实际native537/aggr40通过，D4 caller亦验收；只在真实ordinary kernel/read_input Err记录，保持原始拥有型错误，无持久/祖先猜测site。验证/预算/成功后失败不造site，typed code不解析Display；新reported拒绝Host、旧路径不动。
- [x] (2026-09-28) D4四文件私有overflow view实际10/10，旧D3=10/D1=16/D2=8；完整1371发现/1274pass/4同名同panic/93ignored，父agent核对四SHA与fmt。自身program/spec绑定、1690前全join、own '+'与嵌套PB显示分开，raw cause可回收，单字符串有界渲染与全部正常返回warning端点。独立只读复核无域内发现；不等于一般SQL诊断或公共激活。
- [x] (2026-09-28) C3b已实际native575/575、aggr40、caller1274/4原完整failure block/93ignored通过；9expr manifest276b2d…590b匹配，旧537期望未改。实测EvalFrame400/Program128/Control400/FrameResult176/Node152；真实layout会改变固定byte cap拒绝前缀，不以旧sizeof测试通过冒充绝对预算前缀相同。接受下一effect/发布前retained计量，非hard peak/未知callback预限；旧mode payload政策保持，容许保守minimum过拒绝。
- [x] (2026-09-28) D5前置单文件FieldType observer实际8/8（--lib，428filtered）通过：checked逻辑snapshot payload、无分配/Eq/Hash/clone、独立marker长度、旧memory_usage不变。metadata别名稳定是owner前置条件，不冒充原子预算。
- [x] (2026-09-28) D5 caller五文件实际18/18通过；首次编译仅新测试5处用了依赖不可见cfg(test) Vec转换而失败，改用现有VectorValue::from_scalar，未改断言/产品API。完整caller1292pass/4原完整failure blocks/93ignored（父agent逐block仅线程ID归一化相同）。真实Datum控制、完整独立元信息/producer表、全incoming-schema快照前字节门槛、同次kind/collation检查、own chain及native materialization共存计量已过私有门槛；非public route/general diagnostics/普通调用组合，0族完成。
- [x] (round4，C3c七文件联合验收) signed203-only NativeNumericBatch≤1024 selected occurrences；独立compiled-entry/execution/accounting，整左→整右→parent kernel单lane，同一driver，无N1 NULL短停/重放/root tiling。新增32项与I一项合并RPN608、aggr40、caller1292/4原完整failure blocks/93ignored，实测layout仍400/128/400/176/152。首编译3个新fixture API错误，精确修两处而无断言/产品改动后才执行；corrected manifest413f4f…2894。caller真实入口证明仍必需，无SIMD/公共SQL/族完成声明。
- [x] (round4，D6私有四文件验收) 复用actual native dispatch/Decimal优先/当前flag一次读取、mandatory own program-slot-source chain、完整元信息/实际selection/Int materialization计量；18/18与全量1310pass/4原完整failure blocks/93ignored，父agent逐block仅线程ID归一化相同。初编译缺ProtobufEnum import而零测试，单import修正后才执行。A独立只读对四文件/全部18测试/原生与容量helper未发现域内新增缺陷；不扩张alias-stability/retained-not-peak前提，无probe-replay/public activation/族完成。
- [x] (round5，C4六文件native联合gate) corrected manifest3697a248…bd41；DEV库0/aggr40/RPN636正常+隔离来源1，layout400/128/400/176/152；全TiDB1310/4原完整失败/93ignored逐block对照相同。初次private get测试编译失败零测试，一行改public ChunkRef后才计通过；无旧期望修改。A五文件及冻结worker两段独立源码审阅均无域内新增阻塞，未冒充其执行测试或caller/pool验证。
- [x] (round8，C4私有caller功能/竞态/固定pin Arc请求基准) private wiring后28测试通过但A发现两未覆盖窗口。E只加结构回归与cfg(test) TLS seam；父实际RED1pass/2fail，cached poison确实OkNULL+official1→2，epoch/debt确实拼接无共同有效状态。父仅check_epoch作初末stdMutex/sticky poison及epoch双读修复，实际GREEN3/原28亦通过；A局部复核关闭两finding。最终caller30正常+1独立observerfixture，root unsafe_code=forbid不变；C冻结GNU observer eff83df6…eee18e，父最终ELF8独立样本各真实malloc192+final同ptr free，64clone/empty零，两端safe-Rust m73/r149/c91x1/p64,320 controls齐全；missing-marker真实1testPASS但observer86拒绝。非proxy自证、不宣称malloc显式align、portableArcABI/usable/peak/物理OOM恢复。固定pin接受192 request basis，snapshot caller_arc_measurement_required的重验义务保持；整个family仍0，不含public activation。最终source/ELF/runner/receipt见tidb-c4-final-source.sha256及tidb-pool-arc-final/。
- [x] (round8，opaque runtime failure私有基础) D单新runtime_failure.rs、parent私有mod注册/pinnedfmt，实际7测试通过，A独立窄审无域内发现。原LocalError move进private Arc、Clone共享/identityEq、六outerclass固定native文案、Option phase保留未知、不泄KV公共type/Debug cause；不含context/EvalError/SQL1105/from_evaluation/fold/default接线。最终合并全1347通过/4原完整失败同一/94ignored。
- [x] (round9，native错误承载与terminal映射编译验收) D两文件新EvalError variant/固定1105 arm及2原生origin tests回交；parent三文件native-only reexports/真实EvalError身份trait test。实际carrier8、fullExpr1348/4原完整块仅threadID归一相同/94ignored；session/old-exec/unistore check0。新variant前后renderer均7/1，旧Sequence origin Eq failure完整块仅threadID+临时cfg行366→365归一相同，旧expected不改。首executor编译因旧prepared.rs7处缺clause_message而零测试；parent只补7行generic expression，7fixture前后均过。3临时cfg全部恢复且SHA核对，最终6源码pinnedfmt/diffcheck/A独立复核无域内新增finding。当前ELFb5d49459…872cf4重跑8次actual malloc192/free+负控86，非旧receipt转移。无Columns/ASCII/fold/default/public producer；terminal新arm尚非端到端证明。E/C只读产出下一activation切片和5族选型，所有agent写权均回收；完整族仍0。
- [x] (round10，显式native capability/value API验收) E两文件公开原opaque policy/owner/execution/scope/Sized wrapper、63普通forward+2有效capabilities；discovery失败隔离requested，成功handoff后guard/scope/execution一致。D新adapter carrier与terminal原Pool/Scope/Bridge cause、9native固定消息；parent4边界注册。实际tikv107/1ignored（37caller含7public新项、8adapter、8runtime）、真实publicvalue→terminal2通过；renderer9/1旧完整块仅threadID+3行365→368归一相同；全Expr1363/4原完整块仅threadID相同/94ignored、3下游check0。A最终8源前后hash一致、无新增finding；pool/layout/ledger不改、旧assert不放宽。5site捕获Prepare1/Observe3/Invoke1，真实Prepare/Invoke资源失败已测，Observe只结构覆盖；primary同Arc/phase保留。最终ELFdec586c4…4df4b4实际8×192/free+负控86重验。无默认policy/session/SQLdispatcher/native删除，value→terminal不是SQLquery/network；族仍0。
- [ ] (M5/C4最终验证后补) round12已完成ASCII SQL接管与旧算法删除、实际1slot/0slot列SQL及real kernel dispatcher witness，功能计1/245。仍后补业务operation scope递归传播/既有catcher内guard/全入口复用、150行paired差分重跑和release性能；不存在的ASCII PB/unistore签名不为计数临时扩域。历史worker config Arc双pin88B与caller Aug Arc192B是不同测量域，06改了backend后不冒认旧receipt适用新artifact；F reservation非factory high-water。依用户加速要求这些不再阻塞下族功能迁移，但整体最终验收仍开放。
- [x] (round11，session-runtime-lifetime-05核心验收) D executor2文件optional carrier/defaultNone/COW，4新测试和整个context23通过；E session6文件一次性显式policy/独有稳定root、outer marker+captured closer、nested borrow与Rows转交、Next unwind、finish/retain/Drop，旧生命周期14→28全部通过。父named OwnerError::into_eval_error新增1通过；初始From导致旧vector推断E0282/ZEROtests已撤，不改旧builtin。完整Expr1364/4旧完整块/94ignored，renderer9/1旧块，两者只threadID归一仍一致。TLS仅native epilogue结构性unwind；真实begin失败未自然触发。依用户加速决定不追加穷尽审计/allocator重验，最后192-byte观测仍属04，未冒认当前artifact。精确命令/后补项见session-runtime-lifetime-checkpoint.md。无SQL激活/默认policy/native删除；正在并行下一实际激活与后端函数批次。
- [x] (round12，ascii-sql-activation-06核心验收) SQL ASCII真正接管：func传ctx、string_fn原first-byte/NULL算法删除；scope→execution→无cap TiKV one-shot，全部为同一TiKV实现，不native fallback。仅one-shot有RAII close，coercion/arity/charset/return cast保留。明确实验policy1/1、pool8MiB/W1MiB/F2MiB、64steps/16frames/call usizeMAX，实际2MiB输入通过，不冒称Go默认/物理heap。D6新dispatcher tests全部通过、E实际SQL15通过（非folded列NULL/空/FF/UTF8，1slot正确，0slot所有含NULL都typed1105拒绝）。C5文件共享6op后端，ASCII旧API薄wrapper；Jan local176/1ignored+guard1、Aug fullExpr1370/4旧完整块同一/94ignored。新增5族仅backend-ready不计迁移；功能delegation/deletion进度1/245，严格最终验收另列，性能/全入口复用/分配新测后补。
- [x] (round13，shared-bytes-five-07) 5族caller已接：LENGTH/OCTET_LENGTH、BIT_LENGTH、LTRIM、RTRIM、UNHEX；现同一pool按op匹配/替换cached与idle，实际retire后释债，不开新root绕cap。Public BuiltStringLength eval_in及旧eval→NoColumns、typed row已接，原stored_bytes→coerce_str/invalid ENUM-SET语义与trim/UNHEX包装保留，native算法删除。真实dispatch4通过（含2MiB bytes）、SQL17通过（1slot混合op+0slot12调用）、fullExpr1374/4旧完整块/94ignored。新SQL fixture首次16/1仅Bytes标签错误，依据未改chunk row.rs64–76 SetString修成String+Binary，payload/旧预期/生产代码不变。功能进度6/245；07九源已stage快照，后续08源不入本次提交。
- [x] (round13，shared-text-four-08) CRC32、REVERSE、CHAR_LENGTH/CHARACTER_LENGTH、QUOTE四完整族已接并删native：C仅既有5后端源扩6op（byte/UTF8双支），仍单Bytes输入，Quote(NULL)官方返回Some("NULL")；D获9expr+1unistore源（见活动权），保留CRC32 UInt包装、reverse/主charlength Go逐坏字节normalize、QUOTE本来的Rust from_utf8_lossy。PB CharLength删计数与针对该路径NULL捷径；legacy SimpleSig CharLengthUtf8保留原Rust normalize后走public helper/C4，不擅改坏序列语义。E仅lifecycle.rs少量实际SQL。父Jan180/1ignored+guard1；Aug新dispatch2/SQL19/legacy1/fullExpr1376+4旧完整块+94ignored均实际通过（full exit101）。SQL首次18/1仅new fixture误认CRC32 SQL unsigned：原rewriter int()为signed，父据原源码修new expected，raw UInt/所有payload/生产inference不动。主/PB Go坏后缀计2、legacy Rust计1均验证。功能进度10/245，两仓08验证源已stage（TiDB11/TiKV5），09WIP不入提交。
- [x] (round14，fixed-args-five-09) HEX/BIN/LEFT/RIGHT/REPLACE五完整族核心验收：Jan186/1ignored+guard1；Aug dispatch3/SQL21/fullExpr1379+4旧完整块+94ignored，无新测试失败/预期修改，功能进度15/245。C同5后端源新增EvaluatedArgs::{Bytes,Int,BytesInt,Bytes3} +eval_args（eval_one为薄Bytes兼容），op HexInt/HexStr/Bin/Left/LeftUtf8/Right/RightUtf8/Replace，固定1–3ColumnRefs+FnCall，不开放任意program。D仅5expr {tikv/evaluated_ascii,tikv/evaluated_ascii_tests,tikv/mod,func,string_fn}.rs；evaluate_args_in/run_args为唯一driver，旧Bytes薄转，同一pool/root/epoch，max_nodes2→4支持固定3参，现steps64/frame16不变。E仅lifecycle.rs少量1slot混合/0slot直调拒绝（不外套HEX遮蔽native旁路）。HEX须7020/7021两支保留声明类型/BIT/UInt/舍入；BIN7002保留warning1292及转换；LEFT7028/7027、RIGHT7048/7047保留count-first与Go text normalize；REPLACE7044保持tuple所有coercion顺序、空needle/非重叠替换及subject packing。暂缓CONCAT/WS（variadic/lazy/packet）、SUBSTRING（MAX len溢出旧空串vsKV饱和差异）、INSERT_UTF8（字符位置当byte offset）、LPAD/RPAD（空pad/UTF8长度限不同）、STRCMP/search；不凭同名/半域凑数。
- [x] (round14，integer-seven-10) 七完整整数族bit_count/bitneg/bitand/bitor/bitxor/leftshift/rightshift核心验收：Jan190/1ignored+guard1；Aug bit_dispatch3/SQL23/fullExpr1382+4旧完整块+94ignored，无新失败/expected修改，功能22/245。C既有5源新增Int2及7官方op（3128/3121/3118/3119/3120/3129/3130），同driver，无数值窄化；Jan及全部caller核心已通过，Int2无需新driver/limit/metadata域。D精确6源expr/{tikv/evaluated_ascii,tikv/evaluated_ascii_tests,string_fn,ops,ops/integer_coerce,ops/real_coerce}.rs，删count_ones、全部unary/Int/Real/Decimal bitwise/shift算法；保留原NULL点、cast顺序、双Result先求值与Decimal warning先后，SQL既有ctx可达，不扩原排除bitops的numeric/vector fast gate。BIT_COUNT signed，六bitwise raw/SQL UInt；E仅lifecycle两个1slot/0slot SQL tests。无原PB/unistore admission，不新增这些签名凑计数。公共NoColumns helper仍TiKV one-shot。
- [x] (boolean-five-11核心验收) 五完整BOOL族NOT/ISNULL/ISTRUE/ISFALSE/ISTRUE_WITH_NULL：Jan192/1ignored+chain guard1；Aug boolean_dispatch3/SQL25/legacy1/fullExpr1385+4旧完整块+94ignored，功能27/245，无新失败/old expected修改。原frontend一次truth/presence→Int None/Some0/1，不缩窄SQL输入类型；C新增UnaryNot/IsNull/IsTrue/IsFalse/IsTrueWithNull及三negated固定base+UnaryNot双FnCall，严守ordered sig/name/fn_ptr/arity witness，no arbitraryprogram/no fakeNULL。D精确9源expr/{tikv/evaluated_ascii,tikv/evaluated_ascii_tests,lib,ops,func,builtin_ext/compare2,scalar_function,scalar_function/pb_builtin}.rs+unistore/cophandler.rs，closed BooleanFunction+eval_boolean_ready_in供legacy，max_depth2→3/nodes4不变；删native NOT/bitmap快路，child前退回已有ctx row fallback，不扩admission。AST/typed UNKNOWN、普通truthy/PB1292、uni child0/eval_datum差异原样保留。NOT IN/BETWEEN/LIKE/REGEXP仅否定一步同kernel，基础谓词/child顺序不动。E仅lifecycle追加现SQL可达13表达式、NULL三态+直接WHERE guard；ISTRUE_WITH_NULL原SQL rewriter不admit，不编造SQL，留PB/native验证。
- [ ] 下一12先快速接MD5、SHA/SHA1两完整hash族（别名只算一族）：C同5后端文件仅新增Md5/Sha1官方nullable Bytes→Bytes，不新driver/limits；D已批精确3源expr/{builtin_ext/crypto,tikv/evaluated_ascii,tikv/evaluated_ascii_tests}.rs，保crypto hash_input及小写hex文本packing，删仅这两族使用的hash_unary计算/Md5/Sha1 imports；hash_input/hex_lower的SHA2/SM3/PASSWORD消费者保留，parser/auth独立PASSWORD SHA1算法不冒称消失，不native重试OpenSSL错误；E仅lifecycle追加1slot/0slot三spellings/NULL/空/二进制列。候选余5族暂不计：SHA2非法bits原静默NULL而KV多1583；UNCOMPRESSED_LENGTH需真实1259事件；UNCOMPRESS还1258/1259、bounded StreamEnd与空输出差异；COMPRESS Go-deflate实际bytes不同；ORD NULL→0及charset差异。不得只正常子域接管或假装zero-warning worker已交付诊断。
- [ ] 后续数学/布尔候选需先闭合完整域：PI要真NoArgs+owned Real，不fakeNULL；Real=NotNan允许Inf不允许NaN，现native却接NaN，不能unsafe NotNan/只计有限域/host直接补结果。ASIN/ACOS/SQRT/SIGN、RADIANS/DEGREES各有NaN/Inf/1690差；EXP/LOG10及trig有Go ULP差，LOG全1/2arity需保3020warning，现sealed worker零warning且1105失败适配不等于1690。五布尔族已按前条以显式truth/presence规范化完整接，不再受原bitmap/任意ET障碍；其余AND/OR/XOR仍保原eager/lazy和诊断区别。不能把单Real臂算完整族；不改245分母。
- [ ] 接入 TiDB 全部求值入口，包含 PB typed builtin 和 unistore `SimpleSig` 旁路。
- [ ] 按函数族迁移、补齐 TiKV 缺失纯 kernel、删除 native；收紧并记录例外。
- [ ] 达到覆盖目标，完成 SQL/诊断/缓存/并行验证、性能记录、去重审计及仓库验证门槛。

## Execution priority update — user-approved fast Demo

The user explicitly approved the synchronous evaluator/pool design and requested substantially faster progress. Prioritize working SQL delegation and deleting duplicate native kernels, then batch additional families. Keep compilation, existing relevant semantic tests and a few core lifecycle tests as immediate checks. Exhaustive panic/race matrices, repeated allocator/cohort measurements when layout is unchanged, comprehensive per-cut independent audits and release performance work are follow-ups, not prerequisites for each functional cut. Preserve existing tests; fix observed material failures, but do not grow speculative test infrastructure before activating functions. Record deferred checks honestly; do not weaken expected SQL results or introduce native fallback. The ≥90% final migration objective and paired commit/push policy remain. Earlier stricter per-step gates in this document are superseded by this sequencing decision; final completion/PR requirements remain separately stated.

Immediate sequence: finish the already-written session lifetime slice with focused checks, push it, then activate SQL ASCII and remove its native byte algorithm; reuse that route for the next function batch. No more foundation-only rounds merely to perfect accounting evidence.

## Context and Orientation

### 工作区、基线及不得修改的旧实验

工作区根目录是 `/home/agent/tidb`，它不是 Git 仓库。两个新 worktree 的计划路径分别是 `/home/agent/tidb/expression-unification/tidb` 和 `/home/agent/tidb/expression-unification/tikv`，两个仓库内的新分支都叫 `experiment/shared-expression-foundation`。路径和分支名相同不意味着两个仓库共用 Git 历史。

实施采用经用户批准的固定官方基线：

- TiDB：`hparser-integration`，`364aef2bab5cc633ecb76a775ae8f36f86a6687d`。
- TiKV：`master`，`548812e1ef57aef077a2062a9cc356640a6347f5`。

原调查 TiDB 基线是 `42691860cfd969a8451c9076f0a354a3522df4d3`。实施前 `git ls-remote` 和 GitHub compare 核对发现向前两个提交：PD region 重试，以及 catalog/partition DDL/session warning drain 等变更；无 datatype/expr 核心变更。新 DDL warning drain 纳入诊断基线检查。原调查链接保留其原 SHA，不改写成未调查的来源。之后固定上述 SHA，不随官方分支移动自动追新。创建前 fetch 获取提交并验证归属。任何路径或分支冲突都要停止核对，不覆盖、不 reset。

现有 `/home/agent/tidb/expression-reuse/{tidb,tikv,tiflash}` 是只读参考，不修改工作树内容、不继承它们的功能或测试删除。调查时 TiDB 实验 HEAD 为 `4104edb`，TiKV 为 `70592fc`；其中 standalone、borrowed backend、`Any + Sync` 和额外 lazy framework 是实验改动，不是官方能力。旧原始快照位于 `expression-reuse/original-audit-13YhPE/{tidb,tikv}`，TiDB 为 `ceaaa790da06562dbaa0aff48a7fd6914b6375f7`，TiKV 为 `51b411a728f7c5b12f919fd4dac00a664145b751`。这些快照落后于目标，尤其不能漏掉较新的 protobuf typed evaluator。

本轮不创建 TiFlash worktree。未来只记录其 C++ `dbms/src/TiDB/Collation/Collator.h` 接入同一实现需要的 ABI、buffer、生命周期和类型约束。

### TiDB 源码地图

以下路径相对新 TiDB 仓库根目录，行号会变化，以符号和固定基线为准。

`rust/crates/tidb-datatype/src/collation.rs` 包含 `Collator` facade、`Collation` 操作和 `WildcardPattern`；需要保留名称查找、legacy/new 模式等宿主职责，替换其运算实现。`field_type/mod.rs` 保存完整 SQL/schema 类型信息，不能缩成 TiKV protobuf 字段视图。`tidb-codec/src/datum.rs`、`package.rs`、`join_keys.rs`、`tidb-planner/src/ranger/points.rs`、`tidb-chunk/src/compare.rs`、datatype 的 enum/set、统计和索引编码都是 collation 消费者。

`rust/crates/tidb-expr/src/lib.rs` 的 `eval`/`eval_in` 是一套 AST 解释器；`expression.rs::Expression::eval` 和 `scalar_function.rs::ScalarFunction::eval` 是 typed 表达式入口，并有数值 fast path、`eval_by_signature` 和返回类型适配。`func.rs`、`ops.rs`、`cast.rs`、`math_fn`、`string_fn`、`time_fn`、`regexp`、`builtin_ext` 承担实际 native 运算。公开的 `apply_binary`、`apply_unary`、`concat_values`、`date_add_interval`、`avg_of*` 等 helper 也需审计，不能只改 SQL 投影。

目标基线新增了 `rust/crates/tidb-expr/src/scalar_function/pb_builtin.rs` 和 `distsql_builtin.rs`。`ScalarFunction` 可持有 `PbBuiltin`，以保留的 protobuf signature 选择实现，而不是以展示函数名选择。其 `Kernel` 包括 Binary/Logic/IntegerMod/IsNull/Truth/Case/If/IfNull/Cast/String/Round/FromUnixTime/Regexp/Json/Values 等 native 分支，必须纳入替换和删除清单。

`evaluator.rs` 中 `EvaluatorProgram` 保存表达式和输出列映射，`EvaluatorSuite` 保存执行本地的 column-owner 转移状态。向量比较、bool、Decimal 算术绕过普通 row eval。`rust/crates/tidb-executor/src/selection.rs::FastSelectionFilter` 的 NullTest/StringIn/And 又是一组旁路。filter 调度还维护物理 selection 和 EQ-from-IN 的 NULL mask，不能随计算代码一起误删。

`constant_fold.rs`、`constant.rs`、`rewriter`、`build.rs`、`builtin_arithmetic.rs`、`builtin_compare.rs` 包含构造、类型推导和 folding 策略，不应整文件删除。`builtin_registry.rs` 的注册名含别名和未实现项，不等于 evaluator 覆盖分母。`pushdown_catalog.rs` 同时带远程 admission、signature 和序列化逻辑，不是可直接拿来作为本地 compiler 的完整实现。

executor 的 projection、selection、join residual、sort/group key、aggregate/window 参数、generated/default column、DML、partition pruning，以及 planner 的 constant/range/plan-cache 和 session 的 AST 调用方均需纳入入口清单。`rust/crates/tidb-unistore/src/cophandler/eval_context.rs` 的 `RequestEvalContext`、`SharedExpression` 之外，`cophandler.rs` 还有 `SimpleExpr::Func(SimpleSig, ...)`、`eval_expr`、`eval_datum`；只改 SharedExpression 会留下 native cop evaluator。

### TiKV 源码地图

以下路径相对新 TiKV 根目录。

`components/tidb_query_datatype/src/codec/collation/` 保存 collator、charset 和权重；`src/def/field_type.rs` 保存 `FieldTypeTp`、signed collation 及 protobuf accessor；`src/codec/data_type/` 保存 Decimal、Time、Json、scalar/vector 等运行时值。`src/expr/ctx.rs` 定义 EvalConfig、EvalContext、SQL/请求 flags 和 warning 累积。

`components/tidb_query_expr/src/types/expr_builder.rs` 提供公开 `RpnExpressionBuilder::build_from_expr_tree`；`types/expr_eval.rs` 提供 `eval_decoded`；`types/expr.rs`、`types/function.rs` 定义 RPN 节点、function metadata 和现有 short-circuit 节点。`lib.rs` 映射现有 signature，`impl_*` 是 kernels，`tidb_query_codegen` 生成 rpn function metadata。

已有 `Vec<VectorValue>` 到 decoded `LazyBatchColumnVec` 的转换，不需要复制一个新的 Column enum。`eval_decoded` 要求非空且不超过 1024 个输出行；调用边界必须处理空 selection、切批、索引和类型检查。返回值可能是 scalar、借用列或计算产生的 vector，不能一律 `take_vector_value`；应按逻辑行读取并正确广播/物化。

## Surprises & Discoveries

- Observation: 最新 TiDB 具有 AST、SQL scalar、PB typed、vector/filter 和 unistore SimpleSig 等多条 live 计算路径。Evidence: `scalar_function/pb_builtin.rs`、`distsql_builtin.rs`、`selection.rs`、`cophandler.rs`；替换 Projection 远不足以去重。
- Observation: 官方 TiKV AND/OR 短路不是无条件语义契约。Evidence: `expr_builder.rs` 的 flag、worthwhile heuristic 和 32 层限制，`lib.rs` 尚有 IF/IFNULL/CASE/COALESCE 待补项；超过限制可能退成 eager。
- Observation: 单有 lazy 控制节点仍可能在编译时触发未访问分支的错误。Evidence: `impl_regexp.rs::init_regexp_data` 提前编译常量 pattern；`impl_compare_in.rs` 的 metadata 初始化会移除、交换、截断参数。
- Observation: 官方 function metadata 是 `Box<dyn Any + Send>`，不是 Sync。Evidence: `types/function.rs`、`types/expr.rs`；不要把旧实验的 `PreparedExpression: Sync` 当成现有能力。
- Observation: RPN 按 node 批量求值可能改变 TiDB row-major 的 warning/首个错误顺序。Evidence: `EvalWarnings` 是有上限的顺序错误列表且没有 row tag，不能事后简单排序恢复。
- Observation: binary collation 不等于数值型 binary literal。Evidence: `impl_cast.rs` 根据 constant/column 和 binary literal 来源选择不同 cast kernel；普通文本不能误走数值 binary 解释。
- Observation: 新 TiDB 的函数 metadata cache 与 statement context ID 有关，Clone 清空缓存、失败初始化不缓存。Evidence: `builtin_ext/cache.rs`；不能把带 statement 值的缓存塞进共享 plan。
- Observation: Cargo 不继承依赖仓库的 patches、锁文件或 Rust 工具链。Evidence: 两个 Cargo workspace 独立；TiDB 使用 TiKV path dependency 时必须验证实际解析和 caller 工具链。
- Observation (M0 并行静态审计，运行确认待补): AST `eval_in` 的 AND/OR 先求两边，typed/PB 路径却短路；expr LIKE、datatype LIKE、JSON_SEARCH 存在不同 escape/字符比较行为。Evidence: TiDB `lib.rs:902–920`、`scalar_function.rs:1618–1632`、`scalar_function/pb_builtin.rs:233–246`、`like.rs`、`binary_json_ops.rs`。普通 LIKE 统一到 datatype/TiKV 行为的差异须作为显式修正并补 red/green regression；JSON 的 dangling-escape no-match 以明确 matcher policy 保留，不能静默改写。
- Observation (M0 静态审计): `ScalarValue::from(f64)` 会把 NaN 转成 NULL；FieldType convenience accessors 会截 flags/width 或遗漏 vector variant，TiDB FieldType Clone 可共享 mutable elems。Evidence: TiKV `data_type/scalar.rs:142–175`、`def/field_type.rs:54–74,354–397`；TiDB `field_type/mod.rs:516–532`。必须 checked 映射、完整 detached SQL metadata、显式 literal provenance；PB 参数位解释由 signature 决定，不按 Datum tag 拒绝。
- Observation (M0 静态审计): TiDB Decimal 公共 API 支持超过 9 words，TiKV core 固定 9 words，且负零乘积可能丢 visible scale；SUM/AVG 还有 i128 fast 算术。Evidence: TiDB `decimal_tests.rs:661–740,1175–1225` 和 `hash_agg.rs`；TiKV `mysql/decimal.rs:891–899`。初步 bounded bridge 不是 Decimal 去重完成，wide 域保持未迁移，禁止 string/f64 或 unsafe chunk struct copy 适配。
- Observation (M0 静态审计): raw_varg generated validator 不保证全部异构 child 类型安全；metadata 初始化还覆盖 temporal unit 和 IN union CAST。owned 预转换可能在 dead branch/未选行触发表示错误。Evidence: TiKV codegen `rpn_function.rs:1267–1272`、`impl_regexp.rs`、`impl_time.rs`；这些是 local facade 的独立安全/demand 门槛，不能仅复用旧 validator 就宣称安全。

## Decision Log

- Decision: 唯一实现优先于少改 TiKV；允许必要的 API、kernel、编译和 runtime 重构。
  Rationale: 用户明确反对为少改 TiKV 而保留两份算法。TiDB 适配层只能做职责不同的映射和绑定。
  Date/Author: 2026-09-28，用户要求与主 agent 设计。
- Decision: 使用官方分支创建新 worktree，不继承旧 engine-only 实验。
  Rationale: 旧实验含不同基线、额外运行时和功能/测试删除，不能据此证明当前兼容性。
  Date/Author: 2026-09-28，用户选择及源码调查。
- Decision: 先直接依赖现有 TiKV datatype/expr crates，不预先拆公共仓库或叶子 crate。
  Rationale: 优先验证共享实现，暂时接受重依赖成本；不得通过源码复制、include hack 或全 workspace 重写解依赖。
  Date/Author: 2026-09-28，主 agent。
- Decision: 用官方 RPN 加薄 local 构建入口，不移植旧实验的 standalone/borrowed/lazy 框架。
  Rationale: 现有 RPN 已可嵌入，只补必要能力可以保持单一运行时和清晰 diff。
  Date/Author: 2026-09-28，主 agent 与 evaluator 调查 agent。
- Decision: shared Arc 仅共享 immutable typed/lowered spec，每 worker 独立编译实例和执行状态。
  Rationale: 官方 metadata 不保证 Sync；避免无必要的全 metadata Sync 改造、共享锁或 unsafe 声明。
  Date/Author: 2026-09-28，主 agent。
- Decision: 使用单一 owned/native VectorValue 传输；可能影响诊断顺序的图保守以宽度 1 调用同一 RPN。
  Rationale: 先证明语义，不能保留另一套 scalar/native evaluator 或用事后排序修复已截断的 warning。
  Date/Author: 2026-09-28，主 agent。
- Decision: 至少 90% 完整纯标量函数族作为验收目标，分母预先冻结；实际覆盖尚未测量。
  Rationale: 将“大部分”变成可审计目标，防止仅覆盖少数简单形状就宣布迁移成功。用户未提供原始数值，该数值是本计划的操作化标准。
  Date/Author: 2026-09-28，主 agent 提议并纳入计划。
- Decision: 采用多 subagent 并行，但单文件单写入者，主 agent 负责契约、集成和证据。
  Rationale: 用户要求并行加速，同时要求最终结构清晰；并行不得制造多套实现或覆盖别人的修改。
  Date/Author: 2026-09-28，用户要求与主 agent 执行规范。
- Decision: 文档落盘阶段仅保存本文件，不创建 worktree 或改产品代码；本文件是唯一主计划。
  Rationale: 当时用户批准的执行范围限于文档；该阶段已完成，后续授权见下一项。
  Date/Author: 2026-09-28，已批准的文档落盘计划。
- Decision: 启动 M0–M6 全部实施，TiDB 更新为已审阅并批准的 `364aef2bab5cc633ecb76a775ae8f36f86a6687d`，TiKV 基线不变。
  Rationale: 用户明确要求实施当前计划并批准退出计划模式；增量已核对，固定基线后不持续追新。
  Date/Author: 2026-09-28，用户批准与主 agent。
- Decision: 宽 Decimal 扩展现有 TiKV Decimal 为 Clone/non-Copy、`SmallVec<[u32;9]>` 和宽 counts；保留当前 ScalarValue/VectorValue/RPN 类型，只维护一组显式 Grow/Fixed 策略的数值 workers，物理40-byte cell独立。
  Rationale: TiDB 实际支持超过9word的中间值，不能截断或另留数值后端；bounded MUL 的operand预裁剪不能用exact-then-clamp替代。先完成 Default-safe NULL 容器，再做表示、worker和TiDB facade/native删除三个可编译检查点。详细源码审计见 `evidence/decimal-shared-core-options.md`，唯一B接口冻结见 `datatype-contract.md`；均不是第二ExecPlan或完成证明。
  Date/Author: 2026-09-28，主 agent 审阅E/B设计并放行B2.0安全前置。
- Decision: C2a 控制分类留在私有 SelectedCall/PreparedCall，来自唯一canonical selector的原有arms；不修改公共RpnFnMeta布局或codegen构造。
  Rationale: 避免不必要的宏消费者破坏和第二signature表。以一个官方迭代frame driver完成Int controls/read_input及深树metadata/drop；LegacyWire的32层admission保持，local strict不能深度转eager。DEMO limits明确为frames1024/tasks256/retained64MiB，max_steps保留既有参数/默认；不是生产推荐或外部取消支持声明。
  Date/Author: 2026-09-28，主 agent 修改C2提案并放行C2a；staged Host C2b待其真实编译gate。
- Decision: 首个TiDB caller切片只做依赖闭包完整的叶子/Int控制接口，不自动接管一般表达式、缩小现有域或计算函数族完成率。
  Rationale: D的源码审计发现PB类型投影丢失元信息、fold-disabled仍可能求值、simple CASE可能重复selector，以及scalar NULL-stop和batch eager不同；必须保留wire来源并新增真正不求值的构建边界/明确demand profile，不能把当前C2 controls当成所有SQL严格语义。所有动态native fallback/replay仍禁止。
  Date/Author: 2026-09-28，主 agent 与 Caller D 只读调查。随后 C2a 实际 API 冻结，主 agent 放行 D1 私有 seed 与 B/C 并行实现；该阶段的编译验收尚未完成，不能提前切换公共 SQL 路径。后续实际验收见 Progress。
- Decision: B2.2精确运算用try_*外层codec::Result区分计数/分配错误；数值状态不伪装资源失败。共享parser保留partial value及Ok/Truncated/Overflow/BadNumber/TruncatedWrongValue五态，沿用一个Grow/Fixed worker集，不使用exact-then-clamp或第二数值引擎。
  Date/Author: 2026-09-28，主agent审阅B的具体API后放行，符号稳定后统一加export。
- Decision: B2.2 DefaultDisplay拟以active words<=9且result scale<=255选择legacy格式，否则走完整u32结果格式。允许此前超容量的Overflow零payload进入完整结果格式；已实测81/2/2负零、十个初始化word的示例会从"0"变"-0.00"。只改变这类格式策略，保留status/header/sign/word和SQL诊断政策；其他旧输出变化仍须报告审批，不能扩称legacy/Go等价。旧数值/字节oracle不改，保留显式legacy格式测试和不可变before日志。
  Evidence: 主agent独立pinned Go probe对此状态String/ToString均复现index9/len9 panic；已验收B2.1 rlib probe复现Display="0"。整数Overflow81/0/0两者均"-0"。极端MOD另发现KV与Go差异且独立Fraction验证Go结果；Fixed MOD修正未获批准，Grow走正确精确策略。
  Date/Author: 2026-09-28，主agent依据probes/decimal-boundaries及两份oracle日志裁定；新formatter行为仍等待实际实现验证。
- Decision: C2b通过可选host_services视图接入强制实现catalog/start/resume/cancel的provider，禁止静默默认cancel；D2通过显式PreparationPurpose复用原builder，不能把Disabled fold当作StructuralOnly。二者分别控制运行需求与构建效应，不能相互推导，也不自动改变现有SqlBuild路径。
  Date/Author: 2026-09-28，C2a/D1通过后主agent分别放行C2b与D2-min；provider稳定性、cleanup边界、四文件D2范围见活动台账。
- Decision: C3a只在新profile入口接收精确PlusInt203的两种逐行路径，不把203改写为已接收的222；profile事实按所有节点的source preorder（root0、子节点从左到右）编号，以不可变完整快照检验stale/缺失/乱序记录。C只验证标签/签名/类型一致性，D证明真实PB摄入与原生Datum kind；没有新增cache。旧API/Host/control不变，AST/batch/其他operator不能借该入口偷渡。
  Rationale: typed/PB的NULL需求按真实调用路径不同，native batch必须完整左子树阶段后才进入右子树；1025行嵌套溢出反例证明whole-expression分块会改变首次错误。稳定source/site事实先保留，site-aware非求值诊断报告与SQL激活另设gate，错误variant保留不等于原生诊断已兼容。
  Date/Author: 2026-09-28，主agent审阅runtime-contract C3a/C3b/C3c并放行七文件实现；D3仍只有源/API设计，C3b/c未放行。

## Interfaces and Dependencies

### 唯一计算路径与 TiDB 边界

最终数据流为 TiDB AST/SQL/PB 表达式，经 TiDB 推导和绑定，再经集中 local lowering，进入 TiKV typed local builder、现有 RPN 和唯一 kernels，最后映射成 TiDB Datum/Chunk 及诊断。直接列 ownership 转移和 literal 广播属于表示/调度优化，不需要为了制造“TiKV 调用次数”而多算一次。

在 TiDB 的 `rust/crates/tidb-datatype/src/tikv_compat/` 集中放置共享 primitive/value 适配。在 `rust/crates/tidb-expr/src/tikv/` 集中放置 `mod.rs`、`lower.rs`、`catalog.rs`、`context.rs`、`batch.rs`；这是拟新增结构，不是对现有文件的事实陈述。按需创建，不预建无用途框架。public TiDB Expression、FieldType、Datum API 保留；具体 TiKV 类型和 protobuf 耦合局限于边界。

保留 arity 检查、完整 FieldType、collation derivation/coercibility、rewriter、schema/hash/decorrelation、session 服务和执行器调度。SQL 推导一次生成准确的 signature、cast 和每个嵌套节点的类型。PB 解码保留原始 typed signature，不再按 FuncName 重新推导。区分 signature 指定的参数 signedness 与 wire result 的 signedness/width/scale；保留必要的位解释和表示适配，不能把它误删成重复算术。

远程 pushdown 的授权、blacklist 和可序列化规则不因本地复用而扩张。可提取共同的 signature 描述，但不能直接复用一个会在 lowering 时读取参数/deferred/correlated 值的远程 serializer，然后跨 statement 缓存其结果。

### TiKV local 构建入口

在现有 `components/tidb_query_expr/src/local/` 新增薄 facade。拟定构建描述如下，实际签名可在首次接口检查点按现有 Rust 类型明确化；它必须只有构建信息，不能实现第二个解释器：

    LocalExpr = Constant(shared ScalarValue, FieldType, LiteralKind)
              | InputSlot(slot, FieldType)
              | Call(FunctionRef, typed children, return FieldType, CallMetadata)
              | HostCall(registered slot, typed children, return FieldType)

    FunctionRef = TiPb(ScalarFuncSig) | Local(LocalFunctionId)
    compile_local(spec, schema, compile_context) -> Result<LocalProgram>
    LocalProgram::eval(state, decoded_columns, selection, host) -> Result<VectorValue>

`LocalFunctionId` 是 TiKV 所有的闭合本地 registry，不是伪造的 tipb ID，也不是全局 plugin 框架。缺失的普通纯函数移入 TiKV `impl_*`，用既有 `rpn_fn` / RpnFnMeta machinery 注册，再删除 TiDB 实现。wire/local builders 共用 prepared-call helper、validators、metadata 和 RPN assembly；该 helper 返回验证后的 opaque prepared call 及 retained/reordered 参数映射，不能把不受约束的 `Box<Any>` metadata 构造权交给调用方。

local function 和 HostCall 不序列化到远程。local facade 必须检查 schema、类型、索引、行数和结果形状，再调用现有可能 assert/使用 unsafe loader 的内部接口。空 selection 不执行任何 kernel、warning、RNG 或 host 操作。超过 1024 个 selected occurrences 切批，但保持逻辑顺序和重复项。scalar 结果广播，column reference 按逻辑行取值，计算 vector 正确物化；literal-only 表达式即便没有输入列，也必须传正确输出行数。

### context、缓存和诊断接口

以现有 `Columns`/statement context 和 unistore `RequestEvalContext` 为宿主所有者，显式映射 SQL/type/error flags、时区、charset/collation、division precision、packet limit、statement clock 和 warning 限额。新增必要的 TiKV 配置项时保留旧调用默认行为；不能用一个巨大 `caller_is_tidb` 分支复制整套算法。

参数、correlated value、当前 INSERT 行和 statement-stable host 值用运行时 typed slots，不改写成永久 Constant。same-type 参数换值只重绑定；类型、完整 FieldType、literal provenance、call metadata、registry/build policy 或编译敏感配置变化使 specialization 失效。rewrite 完成后才冻结 spec；任何 args/ret_type 变更都使旧 compiled instance 失效。不缓存 input row 指针、warning、session 引用或上一条 statement 的 NOW。

主计划中的“编译一次”指每个执行/worker 在可复用 scope 内准备，不指把非 Sync RPN 放入全局 Arc。热循环必须复用实例和 scratch，不能逐行 protobuf 编码、编译或创建默认 EvalContext。旧 one-shot API 可包装一行 RPN；executor/DML 等重复调用方使用显式 prepare/execute 生命周期。

warning 在成功和失败时均从同一执行状态保留：MySQL code/message、顺序、detail cap、总次数和 TryFold 回滚。切批不清空 statement warning；不能只返回一个丢掉错误前 warning 的 Output/Err 封装。unistore DAG、selection、TopN、aggregation 使用同一 request context，响应仅 drain 一次，不能重复或遗漏汇总。结构化适配错误，不能依据错误字符串决定切回 native。

### 多仓库 Cargo 接入

依赖集中声明为相邻 TiKV worktree 的 path，并记录双方完整 SHA。TiDB 顶层集中维护必要的 patches/精确 revisions/lock；TiKV manifest 的必要版本 pin 可以调整。不要盲目照抄旧实验的 revisions，不全局关闭检查，不为了一个依赖改造整个 TiKV workspace。

调查时 TiDB 工具链为 `nightly-2026-08-22`，TiKV 为 `nightly-2026-01-30`；实施前重读实际 toolchain/manifest。TiDB 消费依赖使用 caller 工具链，TiKV 自身仍需在其规定环境通过验证。构建缓存、target 和日志放在新实验自己的目录，避免写入旧参考工作区。Cargo.lock 由唯一 owner 通过 Cargo 解析更新，不由多个 agent 手改。

## Plan of Work / Milestones

### M0：固定基线、接口和完整分母

创建两个新 worktree，先阅读各自规则，确认最新源码，记录与调查基线的相关增量。对所有已实现逻辑函数族、operators、synthetic casts、AST-only 形式、PB-only signature、host primitive 和原本未实现注册项建立清单。每项记录 overload/类型/collation/context 支持域、所有入口、源实现、测试、最终唯一归属和状态。

此清单是 baseline，不能事后删掉难项。迁移状态至少区分未迁移、部分迁移、完整 TiKV-only、明确暂缓、原本未实现；host-only 项单独展示。在新 worktree 改代码前记录可运行的 baseline 测试和行为。验证 Cargo 接入后，冻结最小 LocalExpr、context、value/batch 接口，公布给各责任域。

通过标准：基线和分母有证据，两个仓库的构建失败与本次引入失败能分开识别，基础依赖可以在新环境解析；尚未验证的项明确记录，不把编译失败当作迁移成功。

### M1：collation / LIKE 单实现闭环

以 TiKV collators 为权威实现，补 default/no-pad key 选项，使旧 `write_sort_key` 等入口委托同一个内部生成路径；borrowed 和 owned 输出共用预处理，补最大 key 长度和 raw-key 能力。不在 TiDB 再写 trim、权重遍历、key 编码或长度算法。保留二进制 Cow 借用能力，避免简单二进制 key 不必要分配。

先迁移 legacy/binary、ASCII/Latin1/UTF8/UTF8MB4 binary 和 General CI，UCA 4.0/9.0 通过同样门槛后接入。GBK/GB18030/Pinyin 首轮暂缓但记录残留重复，不冒称 collation 已全部去重。

在 TiKV 现有 collation 下放置 `pattern` 子模块，共用 raw pattern/compiled token 的匹配循环、escape 和字符等价逻辑。TiKV `impl_like.rs` 保留 RPN/NULL/结果封装，TiDB datatype WildcardPattern、expression LIKE 及对应 stringutil 入口委托它。常量 pattern 可缓存，动态 pattern 可流式读取，不因共用强制每行分配字符数组；不可保留两套 native/shared matcher。

保持 PAD SPACE 仅 trim ASCII 0x20；TiKV signed wire ID 不能取绝对值。特别区分 `46` 与 `-46`、`63/-63`、`-45`、UCA `-224/-192/-255` 和 0900 binary `-309`。LIKE 必须按 Bytes/BinaryRunes/CollatorDefined 区分，GB18030 binary 为 bytewise，不能把所有 `_bin` 当作 rune matcher；也不能用整串 sort-key equality 代替字符匹配。

保留无效 UTF-8 的逐操作历史行为，不将 raw bytes 强制通过会校验 UTF-8 的 `SortKey::new`。compare/key/hash 的代数一致性只在适用合法值域验证。TiKV `sort_hash` 不等于 `hash(sort_key)`，不能替换 TiDB group/join/index 的编码协议。若后来处理权重表，GBK 是 big-endian u16，GB18030 是 little-endian u32，不能泛称表都小端。

验证跨 expression、排序、GROUP BY/DISTINCT、JOIN、SET 去重、ranger no-trim key、统计和索引消费者。每迁移一个集合删除 TiDB 生产算法及权重数据；测试对照用原基线/Go oracle，不在最终树保留另一份生产实现。

通过标准：Go key fixture 逐字节一致、compare/LIKE 兼容、借用与 padding 行为正确、原 TiKV tests 不回归，已迁移集合的源码和调用路径只有一个算法所有者。

### M2：类型和值语义

区分 SQL/schema FieldType、kernel 分类、runtime value 和物理 Chunk。保留 TiDB 完整 flags、flen/decimal、charset/collation 名称、ENUM/SET elems、array、Datetime/Timestamp 等信息；checked 投影不能静默截断或按 enum discriminant/结构体大小强转。

先接 NULL、整数、real、bytes，再接 Decimal 及普通 temporal/JSON/vector 值域。值桥接只搬运表示，不能用 SQL string/f64 中转，不能为了适配提前触发未访问分支的 cast、UTF-8 或时间错误。

Decimal 必须共享数值表示和运算核心，wrapper 只保留宿主 metadata。保留结果及 Truncated/Overflow 状态、visible scale、内部计算精度和 declared shape。scalar、SUM/AVG 的共用值算术、比较、group/join key、赋值和 codec 消费者共同验收；不能仅增加两种 Decimal 的转换就宣布运算去重。

普通 JSON/时间表达式进入迁移主线；JSON object 比较/数值容差、TIMESTAMP 时区/DST/zero-date 等按具体反例处理。存储编码和完整 Chunk 布局不强行统一。通过标准是无损表示边界、共享运算和跨消费者行为测试，而不是只匹配类型名字。

### M3：TiKV local runtime 与严格语义

实现前述 local facade，复用官方 RPN，不复制旧实验框架。扩展官方 ShortCircuitFnCall/ShortCircuitFnMeta，使本地严格策略中的 AND/OR、IF、IFNULL、CASE、COALESCE 按 active rows 求参数；消除超过 32 层就改成 eager 的退化，用可增长分支状态/有界迭代在原有表达式资源预算内运行。资源真的耗尽时明确返回资源错误，而不是改变 SQL 语义。

simple CASE selector、NULLIF 首参等只计算应有次数。每个迁移 signature 审计 IN/FIELD/ELT/GREATEST/LEAST 等参数顺序、NULL 和 demand（何时需要参数），不能认为 IF 修好就全部兼容。wire/local 使用同一运行时，wire 入口已有编译策略不无意改变。

metadata 初始化区分结构/type 错误和运行时值错误。常量 regexp 可以缓存成功的预编译，但未访问分支的非法 pattern 不应让整个编译失败；首次真实调用才暴露应有错误。IN metadata 只准备一次，返回参数重排映射，不把执行时绑定参数塞进永久 hash set，不破坏参数求值顺序。typed temporal constant 避免 packed-wire 再解码引入时区捕获。Text/BinaryLiteral 来源显式传递。

RPN node-major 与 TiDB row-major 可能在 warning/error/volatile 顺序上不同。只对证明安全的图启用批宽；其他图用宽度 1 调用同一个已编译 RPN。宽度 1 不修复 eager 子表达式问题，必须同时落实 demand/短路审计。不要通过新的通用 native fallback 追性能。

通过标准：dead branch 不产生错误/warning/RNG/host 调用；深层控制流不退 eager；所有返回类型、NULL 条件、选择行/重复项、错误前 warning 和缓存/绑定测试通过。local registry 至少证明一个没有 tipb ID 的纯函数仍在 TiKV 求值。

### M4：接入全部 TiDB / PB / unistore 入口

`eval_in(AST, Columns)` 改成 lowering 和 binding 适配，不保留递归计算各 builtin 的第二解释器。有 schema 的调用方提供 typed resolver；旧 value-only API 保留其明确的值层规则，不能从 Datum 猜丢失的 schema，尤其 CHAR_LENGTH 的 binary/UTF8 选择。用于求值的 prepare 强制 fold-disabled resolver，避免 eval→rewrite→fold→eval 循环；合法的 folding 策略由 TiDB 管理，计算本身也走 TiKV。

`Expression::eval`/`ScalarFunction::eval` 进入一行 RPN；hot loop 调用方持有预编译实例。保留 Column/Constant/CorrelatedColumn 的绑定和必要表示操作，删除 native numeric fast path、eval_by_signature 算法及 migrated vector kernels。EvaluatorSuite 保留 select-list/row-major 与 column-major 调度、参数读取时机、空 chunk 行为、ColumnSwapHelper；完成计算前不转移直接输入列 owner。

filter 保留物理/逻辑 selection、live-row 过滤次序和 EQ-from-IN NULL mask，但其 compare/IN/NOT/AND/IS NULL 等计算走 TiKV。删除或改接 `FastSelectionFilter` 的运算分支。接通 fold、default、generated、DML、range/pruning、join residual、sort/group key、aggregate/window 参数和公开值 helper；非 expression 消费者所需的 compare/coerce 应委托共享 primitive，不能换个名字继续实现 migrated SQL kernel。

PBToExpr 路径保留 typed signature 和完整 wire FieldType，不经过 SQL 名称重写或类型再推导。参数 signedness 的 signature 选择和返回值 unsigned 位解释是两个边界。对应 native PbBuiltin::Kernel 分支删除。

unistore 的 `convert_expr_with_context` 对已迁移 wire signature 必须选同一个 TiKV program，并删除对应 SimpleSig/eval_* 算法；仅替换 SharedExpression 不够。live DAG 继续用真实 request 的 flags、TZ、division precision、scan FieldTypes 和 warning sink；独立 `convert_expr` 的默认 context API 也保持原契约。TopN/RegionAggregator 等参数计算一起验证。

通过标准：所有入口有执行来源断言，migrated signature 实际命中 TiKV，包括错误路径；不存在 SQL、PB、vector 或 cophandler 悄悄执行旧算法的旁路。

### M5：按函数族迁移、显式例外、同步删除

已存在的兼容 TiKV kernel 直接复用；缺失普通纯函数移入 TiKV 一次，再删 TiDB 实现。没有 tipb ID、lowering 还没接或适配器暂时没写，不是最终暂缓理由。以函数/类型域清单驱动，逐批扩展到普通 JSON/时间等函数，检查 scalar、vector、PB、AST 和 helper。

每个完成域同步删除 AST eval_in/func、SQL scalar、PbBuiltin、vector/selection 和 helper 的 native 计算分支；删去不再使用的 ops、CAST、math/string/time/regexp/JSON 等函数。混合文件保留类型推导、registry 和宿主职责，不整文件误删。不保留 feature-off 或 runtime-toggle 的完整 native backend。

少量真正困难项保留在显式 DeferredBuiltin 清单中，以 local HostCall 接入：只执行登记的那个例外 kernel；ordinary args 先由 TiKV 计算，宿主只在 active rows 上取得可变借用；parents/children 不能因此回到旧通用解释器。例如 add(exception(x), abs(y)) 的 add 和 abs 仍走 TiKV，IF 未访问分支的 host 不执行。

若例外自身需要 lazy 或重复求参数（例如某些 BENCHMARK/宿主扩展契约），先设计分阶段 demand：宿主提出子参数及 row 集合，释放借用，同一 RPN 计算后继续。禁止任意递归 callback 重入同一个可变 HostEvaluator/session。没有正确接入前保留明确的未完成项和旧功能，不以报通用 Unsupported 冒充成功。

通过标准：每项有“旧实现→唯一 TiKV 实现→入口适配→测试→剩余域”的可审计记录，源码不再包含已迁移生产算法，所有既有功能的差异都在测试或精确暂缓项中得到解释。

### M6：集成验收、性能与剩余项报告

冻结最终覆盖结果，对 SQL 值和元信息、warning/error 时机、cache/parallel、索引字节和执行来源做交叉验证。独立 review agent 审核完整调用图与残留 native，主 agent 验证跨仓库集成，不把子 agent 的“完成”直接当最终验收。

记录 direct dependency 的编译成本、owned transport 的复制/分配、宽度 1 的执行开销及 batch 安全域。性能不足如实报告，优化必须继续作用于同一个 TiKV 实现，不能引入旧 native 快捷路径。

完成意味着覆盖目标、必须迁移的核心族和验证门槛均达到，少数剩余项明确且不隐藏。不宣称完成整个 Go package transcreation；若未来工作涉及新的 Go-to-Rust port，仍受仓库要求的完整 Go package 原子清单和验证约束，不能借本试验规避。

## Parallel Execution / 多 subagent 协作

### 调度模型

后续实施默认使用普通后台 `subagent`/`subagent_fork`，保持约 3–5 个真正独立的任务；根据 CPU、内存、Cargo 争用和任务依赖调整，不以 agent 数量作为进度指标。独立任务在同一批中启动，主 agent 同时推进接口、集成或其他独立工作，不忙轮询完成状态。不需要为了几个任务再建设 workflow 编排系统。

角色池如下；角色不等于永久固定 agent，也不表示尚未满足依赖即可开改。

| 责任域 | 主要所有权 | 前置条件 |
| --- | --- | --- |
| Foundation A | 两侧 collation/LIKE 实现、facade、该域删除和测试 | 基线固定 |
| Foundation B | datatype 类型/value/Decimal 桥接及对应值测试 | 初始边界契约固定 |
| Runtime C | TiKV local builder、RPN/短路、context、metadata 接口 | 构建描述及签名契约固定，可与基础层并行 |
| Integration D | TiDB lowering/context/batch 和各执行入口接入 | 依赖 API 可用；接入稳定后可按不重叠函数族分派多个任务 |
| Validation E | baseline/coverage、独立 fixtures、oracle、集成与去重 review | 从 M0 起并行，后续随检查点验证 |

首轮可并行基础层 A/B、runtime C 和 baseline/测试 E。API 闭合后，把资源转向入口接入和不同函数族，runtime owner 处理共享扩展。每个检查点安排未实现该模块的 agent 独立 review。重型构建数量由主 agent 根据资源限流，初始保守串行执行重型 Cargo 集成构建；代码、测试准备和 review 继续并行，不让每个 agent 各跑全量构建。

### 文件归属与接口协作

同一个文件同一时间只有一个写入者。委派必须列允许修改的实际路径、禁止触碰的共享文件、接口版本、依赖、测试范围和完成条件。模块目录职责不能替代具体文件锁：例如 runtime 和函数族可能都需要 `impl_regexp.rs`，必须先分配 owner 或显式交接。

Cargo manifests/lock、共享 `lib.rs`/`mod.rs` exports、公共 registry、生成入口和本主计划默认由主 agent 或唯一指定 owner 编辑。其他 agent 提交所需修改说明，由 owner 集成。不得静默改公共签名、各自补不同 shim、全树格式化、删除他人的修改、自行 commit/reset。所有人在新实验 worktree 中操作自己拥有的文件，不各造一套引擎，也不为每个小任务复制代码。

接口改动先通过消息通知主 agent 和依赖方，更新契约后再实施。依赖未就绪的 agent 可准备测试、审计或其他独立实现，不复制依赖算法绕过等待。锁文件更新和生成文件再生成串行处理；生成产物遵守仓库规则，不手改生成结果。

### 子任务交付、检查点和恢复

每次子任务交付必须包含实际改动文件、唯一实现归属、API/metadata 变化、删除清单、精确 cwd/测试命令、通过/失败/未执行结果、过滤命中数、性能或兼容风险、依赖方须采取的动作和下一步。没有跑测试就明确说未跑，不能用已编译或子 agent 的自述替代行为证据。

主 agent 是本计划唯一写入者。每个检查点在本文 Progress 和后面的活动台账中记录任务/agent ID、owner 路径、接口 revision、依赖、状态、证据和恢复动作。收集仍相关的后台输出，停止已无关的后台 job，避免留下构建或测试服务。恢复会话时先读本文件和活动台账，再检查工作树差异及运行中的 jobs/agents，不能重新派发相同文件给第二个写入者。

当前写权覆盖历史记录：11验证源TiDB10/TiKV5已stage，父发布此快照+文档，绝不重新stage12源码。C12仅TiKV local/{batch,compile,mod,tests}.rs及types/expr_eval.rs；E12仅session/tests_core/lifecycle.rs；D12仅3源expr/{builtin_ext/crypto,tikv/evaluated_ascii,tikv/evaluated_ascii_tests}.rs。A仅11 evidence/boolean-five-checkpoint.md及logs/boolean-five-summary.txt已freeze回父。其余源及两仓guide/主Plan父独占，无agent构建/index/提交权。

当前活动台账（M0 已过，M1/M2/M3 源码检查点推进中）：主 agent 是主计划、Cargo manifests/lock、共享 exports/registry 和集成的默认唯一 owner。goal `goal-013a1489-bf6a-43d4-b9fd-3b27cdb269be`覆盖M0–M6；用户明确“请继续”后已通过工具resume，最新get_goal为revision9、roundsStarted14、phase active、activation armed（用户已批准加速执行策略）（max256，已包含每步双仓commit/push政策）。不得因基础切片通过就标记complete。没有两个 agent 同时写同一文件。当前功能接管且删除native的函数族为27/245（前22族及5布尔族，AST/typed/PB/unistore/helper及原vector路径均闭合；未扩原admission）；严格最终验收计数仍0，性能/全仓等后补，不计12两hash族WIP。基础共享/seed另记。

| Agent / 责任域 | 当前唯一可写路径（相对 expression-unification/） | 状态、接口与依赖 |
| --- | --- | --- |
| `c06804b5-18c6-428d-a426-2437c2752d02` / Foundation/Validation E | 全部产品回交、无写权：原B2.1机械15文件及distsql_builtin.rs +tikv/{lower,tests}.rs的P2修复；新oracle generator/JSON亦回交。D2/D3只读审阅无域内具体发现；D3结论虽经harness失败关闭，后续明确确认完成且未执行/写入，不算测试。Go parser probe已回交，主agent扩至20例实际运行；M5 ASCII全入口源码审计已交；EV-r1与probes/ascii-baseline/source.rs已回交冻结；主agentpaired split-metadata link/150行baseline实际通过。AB-r1完整回执已交回；EV-r2已回交，C4 caller两文件已回交并实际28正常测试通过；round8两个结构性回归及cfg(test) TLS seam已回交；父实际双RED、仅check_epoch修复、GREEN/最终30项通过；零PoolCore测试字段/生产callback；round10同两文件显式publiccap/value已回交并37caller通过、A复核，5phase site/7publictests/typedAdapter mapping，不改pool/layout/ledger，现无写权；parent独占mod/root/context/公共dispatcher wiring，无旧ASCII/其他产品/build/探针改写权；checksum非独立value oracle，按每个Datum断言计证据 | 实际PB-origin别名RED后私有化字段、比较谓词/test-only深拷贝getter；仅迁移测试取值API，oracle不变。GREEN1/seed16/D2八项及完整表达式无新增失败已验收；不是曾观测Int输出错误。独立Fraction/int oracle24对/120值，主agent复核--check/SHA；B2.2-B已选部分自包含数值用例通过，不把全部120值计作核心测试 |
| `99ac2755-78c4-4ecc-aec4-98d2b3d16402` / bounded故障探针恢复 | fresh turn亦失败，parent已interrupt/撤权，无当前write/build权限；parent接管唯一private source并冻结e63c85fe…ab04b | Parent matched TEST datatype e806f510…ff3已link0/run1真实RED：bounded12.34第一malloc8持久HIT后SIGABRT且无RETURNED；observer/control/shape通过，452 compiler deps留档。A追加独立只读；非物理OOM/尚无产品GREEN |
| `3e61e565-0cc2-4374-8924-2273030ed565` / Decimal诊断审计 | 产品只读；新evidence/wide-decimal-consumer-audit.md和tools/decimal-diagnostic-alloc-probe.rs已交回/冻结，无当前写/build权；主agent已成功link并运行bounded实测RED，后续同源relink GREEN由主agent执行 | 审计impl_arithmetic DecimalMod/Divide提前完整Display的有效wide分配风险及邻近调用，提出保持旧bounded错误文本/警告顺序的最小lazy/bounded方案；新wide诊断政策须另裁定 |
| `a39c5e1c-0feb-42b3-9e2b-df89971f8170` / D4独立复核 | 无产品/证据写权，任务已结束 | 只读完成四文件与原生纯显示事实/报告上下文审阅，未发现域内具体缺陷；未运行测试，不扩大父agent实际gate声明 |
| `0056bbcb-8050-4c74-8a5e-279befcecc87` / Foundation A | 原collation产品及EV独立evidence/evaluated-value-review.md已回交冻结；D6只读审阅已无域内新增发现回交；新tools/arc-eval-config-alloc-probe.rs已冻结回交；父agent两pin实际compile/run均0，8+8样本各1×88B及复用/匹配free/control通过，仅固定配置分配范围。当前仅C4冻结五文件+worker独立只读审阅；五文件及batch完整扩展均无域内新增阻塞已回交；K probe及产品追加只读均无域内发现回交；round7 caller只读发现两个atomic/Mutex poison窗口，其余owner/63forwarders无第三阻塞，等待E新冻结局部复核，无产品/doc/build写权 | A-M1-r3 第一admitted-domain共享+删除检查点接受：17/25/9/Go1/LIKE11/util13/cache1/JSON18及postprune61通过；五图删除2,128,092bytes。GB/charset/安全域matcher等残留明确，非whole-collation/perf/package完成 |
| `8e1e365a-3ef3-46cb-9e5b-9443219cdda1` / A 的 helper | 无活动产品写入 | source checker/stringutil 已交付，所有产品权回交；不自动重新派发 |
| `da7edd1e-a3e6-4923-8cff-bf5637e3128e` / Foundation B | H两文件已378/575/40验收，convert.rs冻结。**B2.2-I三文件已联合验收回交冻结**：datatype381/RPN608/aggr40、caller1292/4原完整失败/93ignored与同源matched TEST probe GREEN；J两文件已GREEN源码回交冻结：dispatch RED2→GREEN2/fullDT385、4+18行before/after逐行相同；post-J/C4已实际RPN636正常+1隔离/aggr40/caller1310+4原失败+93ignored，联合gate关闭；Datum RESULT单文件提案已提出；新fault probe回交前B连续harness失败，已interrupt并撤销写权。当前无写/build权；其source的RETURNED/driver grammar未一致；fresh99ac亦立即失败并interrupt撤权。Parent已读取完整probe，接管唯一private文件，修正observer/returned/finish及GREEN记录索引、校验control请求大小/GNU target并格式化；编译/实际结果见最新evidence，datum.rs仍无产品loan | B2.1 native330/codegen20/expr462/aggr40与caller46已验收。批准同一WordLimit::Grow/Fixed worker、checked try_*外层codec::Result（资源/布局失败不伪装SQL Overflow）、zero-divisor内层Option、精确存储/结果formatter、同一parser的partial value +五态disposition。具体API见datatype-contract；主agent负责export。不得wide值先进入尚未闭合的narrow trait/worker；分稳定检查点编译，不另起数值引擎 |
| `909d3b5d-634f-4433-a00c-4af8e7352bdd` / Runtime C | **C2b 已验收回交**：local/{batch,compile,host,host_tests,mod,runtime,spec}.rs、types/{expr,expr_eval}.rs共9文件；types/mod.rs未改已还。**C3a七文件亦已验收回交**：local/{profile,profile_tests,mod,compile}.rs、types/{function,expr,expr_eval}.rs。**C3d五文件已验收回交**。C3b9expr与DT2 helpers已joint gate验收/回交冻结，全部子writer/audit交回。**C3c七文件检查点已联合验收**：corrected manifest413f4f…2894/32新测试。**C4六文件已一致回交，全部writer停止**：local/{compile,batch,mod}.rs、types/{expr,expr_eval}.rs及impl_string.rs严格cfg(test) hook。父agent DEV库0/aggr40；首次RPN单新test误用private get而零测试，一行换既有public ChunkRef并反向SHA证明无其他改动；corrected manifest3697a248…bd41，实际636正常+1隔离来源/aggr40/全TiDB1310+4原完整失败+93ignored及layout不变已验收；29新增=28正常+1隔离。新tools/pool-arc-observer.c eff83df6…eee18e已冻结回交；GNU/glibc真实caller Arc::new观察，14marker与前后safe-Rust controls；父gcc14实际编译/新最终ELF8×192请求及negative86通过，C仅只读receipt复核，无写/build权 | 最新联合native608/aggr40与完整caller1292/4同原完整失败/93ignored已验收；实际frame计量不承诺固定byte cap前缀与旧layout相同。可选host_services视图（默认None），advertised LocalHostServices必须实现catalog/start/resume/cancel；无host程序不调用hook，旧D1两方法ABI保持。单一官方driver、新opaque HostCall节点、singleton signedLongLong、Fresh/Reuse、cleanup/typed catalog tests；不得native eval闭包/假TiPbID/第二解释器。要求provider稳定、cancel无panic/诊断；不宣称外部取消/全部host heap限额/实际SQL宿主迁移 |
| `d07e78a7-abe2-433e-9164-a49ca359a0f0` / Caller D | D1产品冻结；**D2-min 已验收，四文件回交**：rewriter.rs、new_function.rs、新rewriter/{preparation,preparation_tests}.rs。**D3六文件已验收回交**，10项通过、旧16/8与全量无新增失败。**D4四文件已验收回交**。D5 observer单文件8项已验收冻结；D5五文件亦18项/完整1292+4原失败+93ignored验收回交，主agent记录source manifest/fmt；**D6四文件已18/18、full1310/4原完整失败/93ignored及A独立只读review验收回交冻结**：evaluator.rs、scalar_function.rs、新evaluator/{numeric_batch,numeric_batch_tests}.rs。round8单新runtime_failure.rs已回交，父private wiring/7测试及A窄审验收；原cause opaqueArc/identityEq/六class/Option phase基础已过；round9仅context/terminal renderer两文件已交回，carrier8及全Expr1348/4/94、renderer前后7/1同块与A审阅通过，round10新adapter_failure及terminal两文件已回交，8adapter及真实publicvalue→terminal2项通过、A复核；仅Prepare/Pool两条低层链，SQLquery/session仍未接；现无产品写权；own lowering-contract的ASCII入口/删除方案仍只作后续公共激活依据，不重复E的pool；公共consumer仍Native，无probe/replay/ColumnSwap/leaf/row旁路、无D3/D4/D5/bridge/Cargo/公共激活权 | bounded shape/type preflight、显式PreparationPurpose经同一builder递归传递、opaque结构产物/声明元信息，保持SqlBuild原行为。8项零求值/警告/绑定/metadata/限制/冻结D1衔接测试；CAST只准备不执行，simpleCASE与普通调用profile仍拒绝。D3仅借用D1 common helpers，不改其walk/domain；Cargo/lib/公共路由不动。D1别名缺陷由E独立修复并实际RED→GREEN，D未触碰；D2已8/8及全量比较无新增失败 |

已完成的基础命令：fetch/worktree `bash-18..21`、fresh-worktree Bazel `bash-22`（保留必要生成 BUILD）、locked fetch `bash-23/24`、两侧原基线与caller精确pin均已收集。最初 CMake/Abseil 构建失败通过独立 helper 环境变量解决，没有 vendor patch。TiDB 原四个expr失败各自复现；原 Decimal 58-test filter 有一项20秒超时，隔离后其余57通过。详细 cwd/参数/计数/失败原因统一保存在 `evidence/validation-baseline.md`，不能把这些已知失败和timeout说成全绿。

本次集成 `bash-34..41` 均已收集：KV collation17通过，Decimal初测32/33发现旧零乘积期望冲突；Go oracle4例证实修正并使33/33通过；codegen20通过，expr首次编译6错修复后438通过；caller bridge/col/LIKE等按上述计数通过。一次跨cwd `--manifest-path` 组合（bash-37）已主动取消，避免继承TiDB的Cargo配置，不采信其结果。util第一次误用不存在的 `--test all`，exit101仅是命令选择错误，改为实际声明的 `--lib` 后13通过。绝不把零命中/被跳过/错误shell状态当通过。Crypto/compression16通过，3个原有占位测试仍ignored。

两仓库的维护说明已按实际第一切片更新：TiDB `docs/agents/architecture-index.md`，TiKV `doc/maintenance-guides/src/coprocessor.md`。全仓lint/clippy、SQL/executor广泛消费者、性能和最终去重验收尚未完成。原计划文档审阅 agents `4b7f4c7d...` / `ade9fbcb...` 已完成且只读，没有延续产品写入权。

## Concrete Steps

### 本轮已经执行的文档操作

在 `/home/agent/tidb` 执行 `pwd; ls -la` 确认根目录并观察目录；使用文件工具确认主文件原先不存在，完整阅读参考 `expression-reuse/tidb/PLANS.md`，并检查已有 manifests 的测试入口作为参考。执行 `date -u '+%Y-%m-%dT%H:%M:%SZ'` 得到 `2026-09-28T05:41:37Z`。随后用文件写入工具创建本文，并由主 agent 使用 `read` 分段全文复读、`grep` 核对标题、Progress checkbox、授权状态和关键覆盖词。两名 subagent 完成上述独立只读审阅，主 agent 用定点编辑更新审阅状态。没有运行 Cargo、Git fetch/worktree add、Go 构建或产品测试。

### 已执行 M0：创建前的只读检查和 fetch（历史记录）

以下为已完成M0的复现命令说明，**当前worktree/branch已经存在，不要恢复后重建或reset**。恢复时先核对status/HEAD与上述活动台账。Git的 `-C` 路径是已有仓库；旧reference工作区保持只读，不能清理其修改。

    git -C /home/agent/tidb/expression-reuse/tidb status --short
    git -C /home/agent/tidb/expression-reuse/tidb worktree list --porcelain
    git -C /home/agent/tidb/expression-reuse/tidb branch --list experiment/shared-expression-foundation
    git -C /home/agent/tidb/expression-reuse/tikv status --short
    git -C /home/agent/tidb/expression-reuse/tikv worktree list --porcelain
    git -C /home/agent/tidb/expression-reuse/tikv branch --list experiment/shared-expression-foundation

    git -C /home/agent/tidb/expression-reuse/tidb fetch https://github.com/pingcap/tidb.git hparser-integration
    git -C /home/agent/tidb/expression-reuse/tidb rev-parse FETCH_HEAD
    git -C /home/agent/tidb/expression-reuse/tikv fetch https://github.com/tikv/tikv.git master
    git -C /home/agent/tidb/expression-reuse/tikv rev-parse FETCH_HEAD

fetch 会更新仓库元信息，但不得改旧 worktree 文件。若 fetch 结果不同于本文基线，审阅后先更新本文 SHA 和下一组命令；不能把调查版本和实际版本混写。

### 后续实施 M0：创建固定基线 worktree

工作目录 `/home/agent/tidb`。只有确认路径/分支不存在、基线已固定，才执行：

    mkdir -p /home/agent/tidb/expression-unification
    git -C /home/agent/tidb/expression-reuse/tidb worktree add -b experiment/shared-expression-foundation /home/agent/tidb/expression-unification/tidb 364aef2bab5cc633ecb76a775ae8f36f86a6687d
    git -C /home/agent/tidb/expression-reuse/tikv worktree add -b experiment/shared-expression-foundation /home/agent/tidb/expression-unification/tikv 548812e1ef57aef077a2062a9cc356640a6347f5
    git -C /home/agent/tidb/expression-unification/tidb rev-parse HEAD
    git -C /home/agent/tidb/expression-unification/tikv rev-parse HEAD

预期两个 HEAD 等于已记录基线，branch 正确，文件没有继承旧实验改动。随后读取新仓库自己的规则、toolchain、manifest、test aggregation 和相关 Go 包文档。仓库规则若要求新 workspace 在构建前执行 `make bazel_prepare`，必须遵守或记录明确适用的仓库豁免，不能自行忽略。本文不授权在当前文档轮运行该命令。

工具每次 Bash 调用可能使用新 shell，必须显式指定 cwd 和必要环境，不假定上一调用的 cd/export 持续生效。记录 target/Cargo cache 路径和 toolchain 输出，避免落入旧实验目录。出现沙箱或审批拒绝按环境规则处理，不绕过限制。

### 各里程碑的 Rust 验证命令

下面命令在相应接口/测试已落地后执行；过滤结果必须非零，成功退出但运行 0 个测试不算通过。第一次依赖接入由唯一 owner 更新锁，之后验证用 `--locked`；不通过去掉 `--locked` 隐藏未记录的解析变化。

工作目录 `/home/agent/tidb/expression-unification/tidb/rust`，先检查实际 package/test targets；现有集成测试通过 `autotests = false` 和 `scripts/aggregate-tests.rs` 聚合为 `--test all`，不能把源码文件名直接当 test target：

    rustup show active-toolchain
    cargo metadata --locked --no-deps --format-version 1
    cargo check --locked -p tidb-datatype -p tidb-expr
    cargo test --locked -p tidb-datatype --lib collation -- --test-threads=1
    cargo test --locked -p tidb-datatype --test all collation_sort_keys_match_go_byte_for_byte
    cargo test --locked -p tidb-codec --lib collation -- --test-threads=1
    cargo test --locked -p tidb-executor --test all index_entry_go_bytes::
    cargo test --locked -p tidb-expr --lib
    cargo test --locked -p tidb-expr --test all
    cargo test --locked -p tidb-expr --lib distsql_builtin
    cargo test --locked -p tidb-unistore --lib cophandler
    cargo fmt --all -- --check

全局 collation 模式相关测试需隔离或串行。M4/M6 还必须按冻结清单运行 executor/planner/session 的 projection、selection、join、aggregate/window 参数、DML、default/generated column、pruning 和跨执行 plan-cache 测试。在 M0 阅读真实 targets 后，将每条具体命令、对应域和结果写入本文；不能仅以这几条包级命令代替完整入口验收。新增 local/执行来源测试前先确定 target 和稳定命名前缀，注册后确认实际命中。

工作目录 `/home/agent/tidb/expression-unification/tikv`：

    rustup show active-toolchain
    cargo check --locked -p tidb_query_datatype -p tidb_query_expr
    cargo test --locked -p tidb_query_datatype --lib codec::collation::
    cargo test --locked -p tidb_query_expr --lib impl_like::tests
    cargo test --locked -p tidb_query_expr --lib short_circuit
    cargo test --locked -p tidb_query_expr --lib impl_control::tests
    cargo test --locked -p tidb_query_expr --lib impl_compare_in::tests
    cargo test --locked -p tidb_query_expr --lib impl_regexp::tests
    cargo test --locked -p tidb_query_expr --lib impl_cast::tests
    cargo test --locked -p tidb_query_expr --lib local::tests
    cargo fmt --all -- --check

其中 `local::tests` 是本计划拟新增模块，只有建好且注册用例后才有意义。所有返回类型、metadata、row-selection 和 warnings 测试随 API 一起实现。TiDB 引入依赖所用 toolchain 和 TiKV 自己的 toolchain 都需要验证，不拿一侧编译成功替代另一侧。

产品代码最终完成前，按新仓库要求执行 `make lint` 和受影响验证；相关 Go/Bazel 触发条件遵守 AGENTS，必要时先执行 `make bazel_prepare`，failpoint/RealTiKV 测试按仓库流程启停和清理。不要默认跑无关昂贵 sweep，也不要因实验性质跳过必需验证。本轮只有 Markdown，以上构建/lint/测试全部未运行。

## Validation and Acceptance

### 语义、类型和存储证据

复用 Go-oracle fixture `rust/difftests/transaction-tests/fixtures/collation_key_vectors.tsv` 及其 generator、datatype 的 Go key vectors、codec 的 runtime collation mode tests，以及原有 expression source tables。迁移后应保持 key 逐字节一致，包括 no-trim、prefix/common-handle/restored data；索引扫描与全表扫描、ORDER BY/GROUP BY/DISTINCT/JOIN/LIKE range 和统计结果一致。修改 fixture 必须有独立 oracle 证据，不能为迎合新实现重录预期。

所有 evaluator 差分比较值、完整 SQL 返回类型、NULL、unsigned、scale/FSP、错误码/消息/时机和 warning 序列/总次数；只比较打印字符串不够。保留 operand_dispatch、cast、string/time/JSON、constant folding、参数和 vector/selection 等现有测试，将私有 native helper 测试改接公开编译接口，不删除测试来通过。

至少包含下列行为场景：IF/CASE 未访问分支含除零、strict invalid cast 或非法 regexp；NULL 条件和全 NULL COALESCE；deep alternating controls 不触发 eager fallback；simple CASE/NULLIF/volatile 的计算次数正确；Text 与 hex/bit binary literal 区分；Decimal truncation/overflow 与赋值语义；原始非法 UTF-8 按各操作历史契约处理。

对 0、1、1024、1025 行，零输入列的常量表达式，稀疏、乱序和重复 selection，NULL bitmap 与 scalar broadcast 做测试。empty selection 无 kernel/host/RNG/warning，未选行的坏值不被提前 SQL 转换。相同 prepared statement 多次绑定、改变 timezone/sql mode/collation/参数类型、并行 worker、clone/reset/failure retry、TryFold warning 回滚必须覆盖。错误发生前 warning 仍保留，超过 detail cap 的总次数仍准确。

### PB 和 unistore 的不可丢失用例

目标基线已验证存在以下 PB 测试语义，应在公共边界保留并增加 TiKV 来源断言：every_encoder_signature_has_a_typed_decoder；protobuf_signature_survives_display_name_changes；protobuf_cast_preserves_wire_type_without_sql_rewriting；protobuf_mod_signedness_comes_from_the_selected_signature；protobuf_json_replace_reuses_the_selected_implementation_across_rows；protobuf_control_does_not_evaluate_the_unused_branch；protobuf_binary_and_utf8_signatures_stay_distinct。

具体可观察行为包括 UpperUtf8 即使显示名改成 lower 仍执行原 signature；wire Decimal flen/scale/flags 不被 SQL 重写；相同整数载体在不同 Mod signedness signature 下产生对应结果；JSON 常量 path 配合变化行值不能读取旧缓存；binary CharLength 对 UTF8 字节和 CharLengthUtf8 的结果不同。unistore 请求 flags/TZ/division precision、TopN/aggregation 参数和 warning 响应在 SimpleSig 移除后仍正确。

### 来源、删除和比例验收

为 AST、typed row、batch、filter、fold/default/DML、aggregate/window 参数、PB 和 unistore 加测试用 execution-origin 断言或计数。所有已迁移调用（含错误路径）必须实际进入 TiKV；HostCall 只允许命中登记的例外 ID，不存在 migrated native kernel。编译成功或名字出现在 catalog 里不是运行覆盖证据。

独立 review 搜索旧 dispatch、numeric/vector fast path、PB Kernel、SimpleSig、hidden feature fallback 和 helper 调用，沿调用图确认同一计算没有第二份生产实现。列出已删除及因精确例外保留的函数，区分纯 metadata/表示适配与算法。编译缓存不是结果缓存，不能减少本应出现的副作用/诊断次数。

最后输出完整分母/分子、完整/部分/暂缓函数族及域、所有入口状态和核心族结果。宿主专有项及原未实现项仍展示，但不伪装成迁移贡献；不能把困难的纯函数事后归为宿主项以提高百分比。

## Deferred Scope / 暂缓登记

初始候选是 GBK/GB18030 的 PUA/key 差异、Pinyin/部分 UCA 边界、完整字符集转码、SET 某些链路、特定 JSON object/numeric 比较、TIMESTAMP/DST/zero-date 语义、部分有状态 RNG/sequence/lock/扩展函数、全量 Datum/Chunk 物理布局统一和 TiFlash FFI。它们是需要调查的候选，不是整族自动豁免。常规 JSON/时间能力仍推进。

每个实际暂缓项须记录源码位置、signature/类型/collation/context 域、最小 SQL 或 API 反例、Go/TiDB/TiKV 现有结果、兼容成本、残留 native 实现和调用入口、保留功能的方式、移除条件及测试。采用静态显式路由，不在 TiKV 出错之后 replay native。低覆盖、难度或工期压力不能写成已经兼容。

未来 TiFlash 应调用同一实现，而不是复制一份 C++/Rust algorithm。只记录 C ABI version/size handshake、ptr+len、caller-owned buffers、生命周期/线程/错误、ColumnString offsets 含 NUL、fast path 绕过 virtual 的位置；不得跨 ABI 暴露 Rust enum/Vec/结构体布局。Decimal32/64/128/256 不等于 TiKV 的 Decimal 表示。当前不创建空 FFI 框架或承诺 TiFlash 已接入。

## Idempotence and Recovery

文档可重复读取和增量更新，更新者先读现有内容，不覆盖其他人的记录。未来基线检查、测试和只读审计可重跑；创建 worktree 不是可盲目重跑的命令，先核对 path、branch 和 worktree registry。已有目标必须验证归属与 HEAD，不能用 reset/clean 强制恢复。

阶段失败先保留 diff、命令、日志、SHA/toolchain 和失败用例，区分环境/基线/本次代码问题。暂停依赖它的写任务，修正唯一 owner 的实现，不让各 consumer 自行复制算法兜底。任何回滚只作用于已确认由本实验产生的改动，并保留用户/其他 agent 的文件；不自动删除 worktree、分支或工作成果。

跨仓库接口需要双方一起恢复或推进。删除 native 前必须有完成域的兼容证据；临时迁移对照必须有明确退役步骤，最终树不得留下第二套 backend。失败不能通过删除测试、扩大 Unsupported、隐式降级或修改预期值掩盖。

用户最新授权覆盖此前“不自动 commit/push”：**每完成一个经验证步骤，主 agent 连同本 Plan 提交并推送到 YangKeao/tidb 和 YangKeao/tikv 的 expression-unification-demo 分支**。不强推、不自动创建/拆分 PR，不提交未验收文件或无关生成物。旧 expression-reuse 工作树内容保持只读；新 worktree 的共享 Git 元数据可用于正常提交与新分支操作。固定 baseline 保持为祖先，后续 HEAD 随已验证提交推进。

每步发布使用相同 Checkpoint-ID；两个仓库根目录保存本文件的字节相同发布副本，仍以 /home/agent/tidb/EXPRESSION_UNIFICATION_PLAN.md 为唯一编辑源。TiDB 的 docs/expression-unification 保存精选契约、覆盖清单、源码探针与验证摘要；不提交 cargo/target/toolchain/binary/cache/凭证。跨仓推送非原子：TiKV先推，TiDB提交记录对应TiKV SHA，再推TiDB；任一失败如实记录，不能宣称双仓已同步。远端已存在分支只做快进，冲突先审查，不覆盖别人提交。

首个检查点 `foundation-c4-k-01`：已实际推送并验证TiDB22f094c7bc8c055cb166c3aa5c1c493e5143383a、TiKV84cf2396be9c995b930e46d582b44553c538e92e；共享collation/Decimal/RPN基础、D1–D6、C4与K，完整族0/245。当时E两caller文件尚未wiring/typecheck，未提交；无关vendor BUILD.bazel始终排除。

第二个检查点 `caller-private-pool-02`：已推送并核验TiDB0376452e0b6a249a42d622aefda5ea04b525079c、TiKV8729fd2b2bfd38b69acfbf4d6c913224b5f9d3ab；private caller/race修复、opaque carrier、1347/4同原失败/94ignored、实际8×192请求与negative86。只含私有边界，不含公共Columns/SQL路由/删除/性能或族计数。

第三个检查点 `native-error-wiring-03`：已推送并核验TiDB778c981b85cb69c9ab5a8b1ccc81748acdf311af、TiKVa86efce2310c37e5b0e011bafeb26e8cee448e74；native-only公共error envelope与terminal固定1105映射，carrier8/fullExpr1348/4原完整块/94ignored、renderer前后7/1原Sequence origin failure、7fixture缺字段机械补齐且前后7通过、3下游库check0、A最终6源码复核；最新ELF实际8×192与negative86重验。精确命令与未验证边界见evidence/native-error-wiring-checkpoint.md。TiKV产品源码仍未改，仅同步Plan；按TiKV先推、TiDB记录配对commit后推并核验远端，仍不创建PR。

## Artifacts and Notes

本文件是唯一主计划。实施后在其中维护入口/函数/删除/例外/证据清单；体积确有必要时可把机器生成的详细清单和日志放到新实验目录，但本文必须保留口径、结论、路径和恢复所需信息，不能退化成只引用聊天或未知脚本的索引。

引用仅用于核对调查依据，实施不依赖 agent 的私有上下文：

- [TiDB 固定基线 PbBuiltin](https://github.com/pingcap/tidb/blob/364aef2bab5cc633ecb76a775ae8f36f86a6687d/rust/crates/tidb-expr/src/scalar_function/pb_builtin.rs)
- [TiDB 固定基线 PBToExpr](https://github.com/pingcap/tidb/blob/364aef2bab5cc633ecb76a775ae8f36f86a6687d/rust/crates/tidb-expr/src/distsql_builtin.rs)
- [TiDB 固定基线 unistore cophandler](https://github.com/pingcap/tidb/blob/364aef2bab5cc633ecb76a775ae8f36f86a6687d/rust/crates/tidb-unistore/src/cophandler.rs)
- [TiKV 固定基线 RPN builder](https://github.com/tikv/tikv/blob/548812e1ef57aef077a2062a9cc356640a6347f5/components/tidb_query_expr/src/types/expr_builder.rs)
- [TiKV 固定基线 RPN evaluator](https://github.com/tikv/tikv/blob/548812e1ef57aef077a2062a9cc356640a6347f5/components/tidb_query_expr/src/types/expr_eval.rs)

第四检查点 `native-capability-value-04`：已推送TiDB258541a32b6c0e2b0394d47b54d0d7d84be5034a、TiKVada4d28ff32c6cace174b1880b47ed2ad3ef2982；显式public capability/value+typed adapter cause，107/1ignored focused、2 actual publicvalue→terminal、renderer9/1旧failure、全Expr1363/4同原/94ignored及3下游check0；A最终8源码审无域内finding；当前ELF8×192/free与negative86重验。源码/精确命令/未验证范围见evidence/native-capability-value-checkpoint.md；EV-r3只放开此窄面，不含SQL/session/defaultpolicy/native删除。TiKV仍只更新Plan，按先TiKV再配对TiDB的正常推送与远端验证流程。

第五检查点 `session-runtime-lifetime-05`：已推送TiDBcf7514d14f2ea0468a0713f247ee00856241be3b、TiKV0822fac54b38e755ad8d236913da65fa0ffc02de；9个Rust文件，显式session root/执行token/closer及Ctx COW，生命周期14旧+14新=28通过、context23通过、named错误桥1通过；Expr1364/4旧/94ignored及renderer9/1旧。依用户加速决定，未重复allocator观测/广域审计/性能；资料精简见session-runtime-lifetime-checkpoint.md。提交仅此9源、Plan和相应资料，排除D的ASCII激活6文件及C的后端扩展5文件WIP；TiKV本次仍仅Plan。

第六检查点 `ascii-sql-activation-06`：已推送TiDB3bb3185c19d6ac85a2ea735cf076680ddd2f5993、TiKV56099331c02d2d14dd742b422a3456806692e844；TiDB7源真正接通ASCII、删除旧native算法，TiKV5源共享六op准备/执行/owned结果，旧ASCII为薄wrapper。Jan176/1ignored+1；Aug dispatcher6、SQL15、全Expr1370/4旧完整块/94ignored。日志精简为实际运行摘要，不再复制海量重复warnings。功能进度1/245，其他5族仅backend-ready；严格最终计数/性能/广域门仍开放。D/E的07五族caller与SQL新变更在06验证源码stage之后才开始，必须排除本次index以外WIP。

第七检查点 `shared-bytes-five-07`：已推送TiDB5987dacfbba5631fbc87d92580aeea87210086ae、TiKVa8688705d8f26acb671cbd8e2c2cd457f8c199a9；TiDB9源、TiKV产品复用06仅同步Plan，5族native删除且同pool真实op匹配/替换。dispatch4/SQL17/fullExpr1374+4旧+94ignored，唯一新fixture标签错误已依据既有materialization修正，未改旧payload/预期。功能进度6/245，performance/strict final0后补。07已验证index快照与08并行源码严格隔离；下一4族随后接typed PB/legacy全入口，不拿kernel-ready算迁移。

第八检查点 `shared-text-four-08`：已推送TiDBe297c720daf049c83596a0f658205aead968914c、TiKV63679d7db08070cb68ddc49e03c2c0aef09ccde9；TiDB11/TiKV5源新增4完整族（6 binary/UTF8 op），CharLength含typed PB/unistore Shared/legacy，保留Go与旧Rust normalization差异；CRC32 raw UInt但原SQL声明signed不改。Jan180/1ignored+guard1，Aug dispatch2/SQL19/legacy1/full1376+4旧+94ignored；新fixture错误unsigned假设依据原inference修正，不是生产语义修复。功能进度10/245；09多参Int(bits)/BytesInt/Bytes3后端与5源caller、SQL并行但不混入08 index。

第九检查点 `fixed-args-five-09`：已推送TiDB3fc66e2f1f59e2c82f2970142a6e2a8355b897d0、TiKV5309b0682558572f301e5c3ba49da29ac569748c；TiDB6/TiKV5源扩闭合ready Int/BytesInt/Bytes3，同一driver；删HEX/BIN formatting、LEFT/RIGHT slicing、REPLACE scan，保转换/警告/packing与NULL需求顺序。Jan186/1ignored+guard1；Aug dispatch3/SQL21/fullExpr1379+4旧完整块（仅threadID归一）+94ignored，无新测试失败或改expected。功能进度15/245；维护guide更新fixed arguments及owner/capacity边界，wire/readpool不变；10整数WIP不入09源码快照。

第十检查点 `integer-seven-10`：已推送TiDBe19ad08fad7cbf066134d1377611e9020673bdb9、TiKV1416e58ff60cf4dd3d2313a0ecbbef3eccbfcedb；TiDB7/TiKV5源接BIT_COUNT及六bitwise，新增Int2但无新driver/limits；删unary/Int/Real/Decimal全部对应native计算，前端保原coercion/NULL/诊断顺序及Int/UInt metadata。Jan190/1ignored+guard1；Aug bit_dispatch3/SQL23/fullExpr1382+4旧完整块+94ignored。父review ops三源diff；原vectorized_builtin_op_func是duration fixture负FSP panic，不是新增bitwise断言失败。功能22/245，11布尔normalized truth/presence及固定双FnCall另WIP，不进10 index。前端全部操作guard/性能/分配新测/全仓等仍后补。

第十一检查点 `boolean-five-11`：TiDB10/TiKV5源接5完整BOOL族及原negated/UNKNOWN形式。前端原truth/presence规范化不缩窄SQL输入；3negated固定base+UnaryNot两真实FnCall，compile depth3/nodes4，同worker/driver。public BooleanFunction供legacy；删NOT/ISNULL vector native算法，child前decline回ctx row入口。Jan192/1ignored+chain1；Aug dispatcher3/SQL25/legacy1/full1385+4旧完整块+94ignored。13 SQL spelling三态、28direct零slot拒绝；ISTRUE_WITH_NULL无原普通SQLadmission，使用PB/native证据；不把planner可消去的NOT(compare)当强制vector路径。功能27/245，12两hash源WIP不进11 index。

## Outcomes & Retrospective

- 用户明确当前同步evaluator（不是线程）的设计合理，但要求加快：本轮完成后停止把完善计账/测试矩阵当每族前置。以编译+核心语义检查推动真实SQL接管与native删除，边界/性能后补；不隐瞒失败、不引入native fallback。下一ASCII激活与5族共享后端已并行实施，不再多轮只交基础接口。

- round10完成显式nativecap/value窄面，不冒充SQL迁移：7public caller用真实C4/NULL/复用/冲突scope/两类panic/前置错误/PrepareInvoke，8新adapter保留typed原cause，2外crate公共value错到terminal确实通过；107focused通过1ignored，完整1363/4旧failure/94ignored，renderer9/1旧Sequence。A独立逐句审guard切换无gap、8源hash前后一致；原Scope/string指针断言保留并加强sameArc/phase。当前TEST ELF独立8×192/free与负控86，不能升级为heap/peak/OOM证明。Observe只有site结构证据；body必须从boundcap取effective scope，不自动代理另一个captured handle。C发现下一生命周期不能照搬每begin新epoch：nested EXECUTE/IMPORT、PointGet fallback、旧detached Close、subquery内部Close、SET_VAR时序都要测；现有配置不是poolpolicy，下一显式配置接线不猜defaults、不抹旧债务。公开Execution.close目前是调用方discipline，若要类型级借用权限须另定API。整体目标仍active，完成族0/245。

2026-09-28：完成源码调查和整体设计持久化。已明确 collation/类型只是基础，最终须覆盖大多数 evaluator；发现并纳入较新 PB typed evaluator 和 unistore SimpleSig 旁路；确定在官方 TiKV RPN 中补严格语义，而不是移植旧实验。多 subagent 并行、文件归属、接口交接、重构建限流及独立审阅规则已写入。

文档落盘阶段仅交付Markdown，随后用户批准M0–M6并创建两棵固定基线worktree。当前M0已过，第一collation/LIKE共享与删除检查点实际通过：四个LIKE RED已GREEN；五个重复镜像清除2,128,092bytes；跨codec/executor/planner/session/stats/unistore的61项后续gate均通过。TiKV CMake/Abseil环境问题已由helper解决，原TiDB四个expr失败仍保持单独基线记录。初始primitive bridge11、Decimal33、codegen20、RPN438通过，不能将其等同全类型/全运行时就绪。

至round6的回顾：B2.0/B2.1与B2.2-A至I数值/producer/lazy边界、C3c closed signed203批量七文件已联合验收：datatype381、RPN608/aggr40、caller1292 pass/4原完整failure blocks/93ignored；layout再测不变。真实bounded malloc probe同源同TEST profile RED→GREEN，MOD/DIV/DAY各8轮actual1=baseline1，非zero/general allocation bound；第一次误用旧TEST artifact的stale RED及C3c三处新fixture编译错误都保留。D1 seed16、D2八项、D3十项、D4十项、D5 observer八项及caller十八项已过。D6实际batch入口四文件已18/18/full1310+4原完整失败+93ignored及A只读审阅验收冻结。J fallible STORAGE已dispatch RED2→GREEN2/fullDT385及4+18行before/after相同，并与C4六文件联合验证：RPN636正常+1实际隔离来源、aggr40、caller1310+4原完整失败+93ignored，layout不变；一处新fixture误用private get而零测试，public ChunkRef一行修正后通过，原失败保留。A的Arc探针实跑双pin88B/control/复用/free窄门槛；C4六文件独立源码审阅亦已无域内新增阻塞回交。E已按EV-r2回交ONLY2新私有adapter/pool文件（7f9da6aa…dde5 /03fcb4b0…8034），仅syntax parse、未typecheck/mod/公共能力/dispatcher激活，首发布明确不stage；D准备完整入口/删除和opaque native error方案，保留原fold抑制/default remap。B/接替99ac的harness失败后均已interrupt/撤权，parent接管新bounded fail-once probe并实际匹配TEST link0/run1：真实Datum12.34首个malloc8拒绝后SIGABRT、持久HIT且无RETURNED，controls/observer通过；A probe只读已无新增阻塞；datum.rs单consumer复用Display的一次fallible sink已同源真实GREEN及DT387/联合gate通过，仍无publicwide。baseline已paired rmeta+rlib链接，150行观测/全部timed Datum断言及GBK/numeric controls通过，未计release/explicit-scope性能门槛。全局入口未切换、完整族仍0/245。oversized真实诊断/temporal、wide publication、selector-once simple CASE仍未放行；资源错误不是SQL数值status，物理raw域不是严格logical parts。命令、owner、恢复点见Progress/台账与证据。

round8：私有caller实际接入并通过30项，A所发现两并发窗口经E确定性回归、parent实际RED→GREEN及A复核关闭；D opaque failure私有基础7项亦过。最终全1347/4原完整失败/94ignored；GNU observer与safe-Rust controls在最终ELFddb196e8…54b7a9b实际8样本验证单次192-byte请求/final free，missing-marker negative86；C只读复核最终cohort。代码仅private模块，没有Columns动态能力/SQL诊断/公共入口或native删除。第二paired checkpoint为caller-private-pool-02，细节见新evidence/caller-private-pool-checkpoint.md。

round9：error承载推进到native EvalError与terminal1105 arm，原cause/API仍opaque；8 carrier与1348/4/94、3下游库check均已实际执行。首次executor因7旧fixture漏字段零测试，机械补齐后7fixture通过；renderer的1既有origin Eq失败已前后独立实测同块，未改旧expected。新opaque arm尚无真实public producer，不能冒称端到端。A最终只读无域内发现；E给出了具体scope/lifetime/catcher切片，C给出恰好5完整族候选而非凑数子域。当前ELF已实际8样本重新计量192 request/free+negative86，第三检查点native-error-wiring-03。

功能接管/native删除进度27/245（前22个字符串/校验/整数族及NOT、ISNULL、ISTRUE、ISFALSE、ISTRUE_WITH_NULL；别名不重复计），目标至少221；严格最终验收仍未完成。其余函数、类型、stagedHost、诊断、全部入口、性能、全仓门槛继续推进，不宣布整体完成或PR就绪。现已授权按每步验证检查点双仓commit/push，实际远端结果以Git核验为准；仍未创建TiFlash工作树。

修订记录：2026-09-28初版仅文档和独立审阅；随后按用户批准执行M0–M6，完成固定基线、双toolchain/Cargo pin、首批共享删除、Default安全及B2.1/C2a/D1实际集成检查点；B2.2-A/B/C、C2b/C3a、D2-min/D3与PB-origin实际RED→GREEN均已验收，数值及Go解析/shift oracle已冻结；此后已推进至round9的K/C4私有caller及native error envelope检查点，公共激活仍未放行。详细命令、编译失败与机械修复、未验证项统一见 `evidence/validation-baseline.md`，本文件持续为唯一主计划。
