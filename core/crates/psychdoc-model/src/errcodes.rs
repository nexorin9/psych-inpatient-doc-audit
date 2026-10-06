//! **全量错误码表**：每一个码的触发条件、是否可重试、给用户的处置建议。
//!
//! 存在的理由：院现场排障靠的是「这个码是什么意思、谁该处理」，不是靠猜。
//! 早先错误只有一个中文字符串，于是同一句「操作失败」既可能来自规则集写坏，
//! 也可能来自 sqlite 磁盘满——值班的信息科工程师只能整包重装再试。
//! 带上**稳定码 + 处置建议**之后，工单上写一个 `E_SCHEMA_TOO_NEW` 就够了。
//!
//! # 这张表和实现的关系：**双向钉死**
//!
//! - 正向：代码里出现的每一个 `E_XXX` 字面量都必须在 [`ERROR_CODES`] 里有一行；
//! - 反向：表里每一行都必须注明它**在哪个文件的哪个错误类型里返回**，
//!   且那个文件里确实能扫到同一个码。
//!
//! 双向都由 `psychdoc-store` 的 `tests/observability.rs` 断言。少任何一边，
//! 表就会变成一份「写得挺好但没人核」的文档——而这正是它存在的唯一理由被
//! 架空的方式。
//!
//! 放在 `psychdoc-model` 而不是 `psychdoc-store`：model 是**所有** crate 的
//! 下游依赖（parse / rules / graph / fhir / store / ffi / cli 都靠它），
//! 表放在这里才可能被每一层引用而不引入依赖环。

use serde::{Deserialize, Serialize};

/// 错误码归属的处置侧。**决定 CLI 退出码**（`0/1/2/3` 契约的第二维）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    /// 配置 / 用法 / 数据形态侧：缺环境变量、规则集非法、参数写错、状态非法。
    ///
    /// 归这边的错误**重试没有意义**：环境变量不会自己长出来，
    /// 规则集也不会自己改对。让 CI 退避重试只会把一次配置事故
    /// 放大成几百次跑批。
    Config,
    /// 外部依赖侧：EMR 端点连不上、429、5xx、事务被拒、通知发不出去。
    ///
    /// 归这边的错误**通常值得退避后重试**（429 尤其如此：限流不是故障）。
    External,
}

impl Side {
    pub fn as_zh(&self) -> &'static str {
        match self {
            Self::Config => "配置 / 用法",
            Self::External => "外部依赖",
        }
    }
}

/// 表里的一行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorCode {
    /// 稳定码，形如 `E_FHIR_TXN_REJECTED`。进日志、进工单、进 `--json` 正文。
    pub code: &'static str,
    /// 归属处置侧。
    pub side: Side,
    /// 触发条件：什么情况下会返回它。一句话，讲清**根因**而不是复述码名。
    pub trigger: &'static str,
    /// 是否值得稍后重试。`false` 表示要人处理，重试一万次也不会变。
    pub retryable: bool,
    /// 给使用者的处置建议。一句话，可直接贴进工单或交办单。
    pub remedy: &'static str,
    /// 返回这个码的文件（相对 `core/crates/`）。反向核对用。
    pub origin: &'static str,
}

macro_rules! codes {
    ($(($code:expr, $side:ident, $retry:expr, $origin:expr, $trigger:expr, $remedy:expr)),* $(,)?) => {
        pub const ERROR_CODES: &[ErrorCode] = &[
            $(ErrorCode {
                code: $code,
                side: Side::$side,
                trigger: $trigger,
                retryable: $retry,
                remedy: $remedy,
                origin: $origin,
            }),*
        ];
    };
}

codes! {
    // ---- 配置与用法：谁改坏了什么，重试无用 -------------------------------
    ("E_USAGE", Config, false, "psychdoc/src/contract.rs",
     "命令行参数写错、少给必填项或给了子命令不认的选项。",
     "按 psychdoc --help 的用法重写命令；跑批脚本改动后先手工跑一遍确认参数。"),
    ("E_ARG", Config, false, "psychdoc/src/contract.rs",
     "选项本身认得，但取值不合法（方向写错、路径为空、类型不对）。",
     "核对该选项的取值范围；批量脚本里通常是变量没赋值就拼进了命令行。"),
    ("E_IO", Config, false, "psychdoc/src/main.rs",
     "读不到规则集 / 文书包 / 缓存文件，或写不进输出文件。",
     "确认路径拼写与读权限；导出盘满时先清盘再重跑。"),
    ("E_INTERNAL", Config, false, "psychdoc-ffi/src/api.rs",
     "内部错误类型拿不到稳定码前缀时的兜底（理论上不该出现）。",
     "把触发时的输入路径与操作步骤发给信息科；这属产品缺陷，不是配置问题。"),
    ("E_CFG_MISSING", Config, false, "psychdoc-fhir/src/config.rs",
     "EMR_BASE_URL / EMR_CLIENT_ID / EMR_CLIENT_SECRET / EMR_TENANT_ID / WECOM_WEBHOOK_URL 缺失或为空白。",
     "在服务账号的环境变量里补齐缺项后重跑；缺项会一次性列全，不会静默降级为本地假数据。"),
    ("E_CFG_INVALID", Config, false, "psychdoc-fhir/src/config.rs",
     "环境变量都在，但形态非法（URL 不带 https、租户号含非法字符、webhook 不是企业微信地址、PSYCHDOC_LOG_LEVEL 写了不认识的级别）。",
     "对照接口规范改环境变量的写法；本产品不做任何猜测式修正（不补 https://，不把未知日志级别回落成默认）。"),
    ("E_DATA_DIR", Config, false, "psychdoc-store/src/error.rs",
     "定不出用户数据目录（HOME / APPDATA / XDG_DATA_HOME 都没有）。",
     "显式设置 PSYCHDOC_DATA_DIR 指向一个可写目录后重跑。"),
    ("E_CACHE", Config, false, "psychdoc-ffi/src/api.rs",
     "增量缓存文件读不了或写不进（损坏、权限、目录不存在）。",
     "删掉该缓存文件重跑一次全量即可——缓存丢了只会多算一遍，不会算错。"),
    ("E_CACHE_CORRUPT", Config, false, "psychdoc-rules/src/incremental.rs",
     "增量缓存文件里的内容不是本产品写的形态（被别的工具改过或写到一半断了）。",
     "删掉该缓存文件重跑一次全量；缓存被拒时本产品宁可全量重算，也不拿半份缓存凑结论。"),
    ("E_CACHE_SERIALIZE", External, true, "psychdoc-rules/src/incremental.rs",
     "增量缓存写不出去（磁盘满、路径不可写）。",
     "清盘或换缓存路径后重跑；缓存写失败不影响本次审查结论，只影响下次是否重算。"),

    // ---- 规则集与表达式：病案室手写 YAML 时最容易踩的一类 --------------------
    ("E_RULESET_INVALID", Config, false, "psychdoc-rules/src/ruleset.rs",
     "规则集加载期强校验没过：引用未定义文书类型、同层码重复、覆盖成环、缺规范依据、豁免谓词非法。",
     "按报错里的文件名与行号改 YAML；本产品在此时零条规则放行，绝不降级为「无规则=通过」。"),
    ("E_NO_RULE_BASIS", Config, false, "psychdoc-model/src/diagnostic.rs",
     "某条规则没有写院内规范条款依据。",
     "在规则集里给该码补 basis 字段；没有依据的诊断不允许贴进交办单。"),
    ("E_EMPTY_MESSAGE", Config, false, "psychdoc-model/src/diagnostic.rs",
     "规则消息正文为空（即使写了依据）。",
     "补 message 正文；空文案会让交办单上出现一条只有码没有说明的整改要求。"),
    ("E_TYPE_MISMATCH", Config, false, "psychdoc-rules/src/values.rs",
     "数值字段遇到非数字文本（全角数字、空串等）。",
     "文书侧修正导出或规则侧收窄适用范围；全角日期是唯一会给补写预览的分支，其余只给依据。"),
    ("E_DATE_PARSE", Config, false, "psychdoc-rules/src/expr.rs",
     "日期 / 日期时间字段遇到非法格式。",
     "确认导出系统的日期格式（FHIR 要求 ISO8601）；低精度（只到日 / 只到时）会走另一条降级路径。"),
    ("E_REGEX_INVALID", Config, false, "psychdoc-rules/src/expr.rs",
     "规则集里的正则表达式编译不过。",
     "修正 YAML 里的 pattern；本产品返回错误而不是抛异常，规则集不会被静默跳过。"),
    ("E_PATH_INVALID", Config, false, "psychdoc-rules/src/expr.rs",
     "字段路径写不出「件 / 段 / 段内键」三级形态。",
     "按 `s1.患方签字` 的写法改条件里的 path。"),
    ("E_OPERAND_MISMATCH", Config, false, "psychdoc-rules/src/expr.rs",
     "算子与操作数形态对不上（如 between 配了文本操作数）。",
     "把操作数改成与算子匹配的形态；本产品不猜。"),
    ("E_DIMENSION_MISMATCH", Config, false, "psychdoc-rules/src/expr.rs",
     "数值比较跨了量纲（字段是「小时」、条件写的是「毫克」）。",
     "统一量纲再比；同量纲的单位会先换算（2 小时 == 120 分钟）。"),

    // ---- 解析与文书结构：包读得动，但这份件/这批件有问题 ----------------------
    ("E_ENCODING", Config, false, "psychdoc-parse/src/lib.rs",
     "导出包含非 UTF-8 字节（GBK 导出 / 非法码点），已按字节替换为占位符并逐处记偏移。",
     "让信息科用 UTF-8 重新导出；本产品不整批失败，能解析的件照常出结果。"),
    ("E_BUNDLE_JSON", Config, false, "psychdoc-parse/src/lib.rs",
     "文书包正文不是合法 JSON（多半是 HTML 登录页或导出中断的半截文件）。",
     "重新导出；若导出走的是院内接口，请先确认拿到的是 Bundle 而不是错误页。"),
    ("E_DOC_TYPE_MISSING", Config, false, "psychdoc-parse/src/discovery.rs",
     "Composition 缺 type.coding，无法判断文书类型。",
     "补齐文书类型编码；该件已进未解析清单，不会被当作「没问题」。"),
    ("E_UNKNOWN_DOC_TYPE", Config, false, "psychdoc-parse/src/discovery.rs",
     "文书类型码不在本院文书类型字典内（转诊单、会诊单等非住院文书）。",
     "确属非文书件可忽略；若属文书则把类型码补进字典。"),
    ("E_ENCOUNTER_ALIAS_MISSING", Config, false, "psychdoc-parse/src/discovery.rs",
     "件上没有住院别名，无法归组。",
     "检查导出是否带 identifier；跨件时间序与归档核齐都以住院别名为键。"),
    ("E_DOC_ID_DUPLICATE", Config, false, "psychdoc-parse/src/tolerance.rs",
     "同一份导出包里出现两个相同 doc_id（多半是跨系统同名导出）。",
     "人工确认后删掉其中一份；本产品既不合并也不取第一条，两条都进人工队列。"),
    ("E_BIND_NOT_CONFIDENT", Config, false, "psychdoc-fhir/src/read.rs",
     "跨文书实体绑定不唯一（同名文书多份），无法确定按哪一份参与时间序判定。",
     "人工在归档核齐视图的待确认队列里指定；绑定不唯一时整对跳过，不猜。"),
    // ---- 输入形态：不是「文书不合规」，是「这份包/这次调用不能用」 ------------
    ("E_BUNDLE_SHAPE", Config, false, "psychdoc/src/dryrun_cmd.rs",
     "输入不是 FHIR R4 Bundle（resourceType 不是 Bundle、entry 不是数组、正文不是 JSON）。",
     "确认导出的是 Bundle 而不是单条 Composition；本产品不会把非法输入当成空包。"),

    // ---- 定位与状态：件级状态机与区间自检 -----------------------------------
    ("E_STATE_JUMP", Config, false, "psychdoc-model/src/state.rs",
     "件级状态非法跳变（如「缺失」直接跳「可归档」）。",
     "按缺失→草稿→已建件→已收→可归档逐步推进；非法跳变不改变任何既有状态。"),
    ("E_STATE_UNKNOWN", Config, false, "psychdoc-model/src/state.rs",
     "库里没有这件，无法推进它的状态。",
     "先让本轮审查登记这件，或确认件 id 是否写错。"),
    ("E_SPAN_REVERSED", Config, false, "psychdoc-model/src/span.rs",
     "区间起点大于终点。",
     "属产品内部缺陷，请把触发输入报给信息科；正常路径不会产生反向区间。"),
    ("E_SPAN_OUT_OF_BOUNDS", Config, false, "psychdoc-model/src/span.rs",
     "区间越出所在段落的文本长度。",
     "属产品内部缺陷，请把触发输入报给信息科；段落切分与字段定位本应恒在界内。"),

    // ---- 存储、迁移与审计导出 ---------------------------------------------
    ("E_DB_IO", External, true, "psychdoc-store/src/error.rs",
     "sqlite 操作失败（磁盘满、文件被占用、约束冲突）。",
     "确认磁盘空间与目录权限后重跑；本产品不会因为写库失败就改口说「已写回」。"),
    ("E_SCHEMA_EMPTY", Config, false, "psychdoc-store/src/error.rs",
     "目标库是空文件但不是本产品建的库（零页 / 已被别的工具占用）。",
     "换一个库路径，或备份后删掉该文件让本产品重建。"),
    ("E_SCHEMA_TOO_NEW", Config, false, "psychdoc-store/src/error.rs",
     "库文件版本比当前程序新（另一台机器或更新版本写过）。",
     "用与该库匹配的版本打开；不要降级写同一个库，否则会把新版本的数据写坏。"),
    ("E_MIGRATION_FAILED", External, true, "psychdoc-store/src/error.rs",
     "迁移链中途失败（某一步执行报错）。",
     "保留现场库文件后重跑迁移；失败时本产品停在原版本，不会半迁移。"),
    ("E_EXPORT_LEAK", Config, false, "psychdoc-store/src/error.rs",
     "审计导出的标识列（住院别名 / 件 id）里出现未脱敏内容。",
     "拒绝导出是有意的：先排查上游绑定是否把真实住院号写进了别名列。"),
    ("E_BAD_TIMESTAMP", Config, false, "psychdoc-store/src/error.rs",
     "传入的时刻不是 RFC 3339（as-of / started-at 等参数）。",
     "改成 `2026-03-10T09:00:00+08:00` 这样的完整时刻。"),
    ("E_RUN_UNKNOWN", Config, false, "psychdoc-store/src/error.rs",
     "库里没有这个审查批次号。",
     "用 export-audit 不带 --run 列出全部批次，或核对批次号拼写。"),
    ("E_DRAFT_UNKNOWN", Config, false, "psychdoc-store/src/error.rs",
     "找不到这份写回草稿（可能已被清掉或来自另一次载入）。",
     "重新从补写预览生成草稿；草稿不存在时撤回会拒绝，不会误报成功。"),
    ("E_DRAFT_STATE", Config, false, "psychdoc-store/src/error.rs",
     "草稿当前状态不允许该动作（如未发送就撤回、已撤回再发送）。",
     "按当前状态选择合法动作；每次状态迁移都会留审计事件，可回查走到哪一步。"),
    ("E_ENTRY_UNKNOWN", Config, false, "psychdoc-store/src/error.rs",
     "草稿条目指向的件 id 不存在。",
     "重新生成草稿；本产品不会静默跳过指向未知件的勾选项。"),
    ("E_NOTHING_SELECTED", Config, false, "psychdoc-store/src/error.rs",
     "一条都没勾选就发起写回。",
     "先勾选要补写的条目；空勾选在**任何网络调用之前**就被拒。"),
    ("E_DRAFT_ENTRY_UNKNOWN", Config, false, "psychdoc-fhir/src/confirm.rs",
     "确认时引用了草稿里不存在的条目。",
     "刷新草稿后重新勾选。"),
    ("E_WITHDRAW_NOT_SENT", Config, false, "psychdoc-fhir/src/confirm.rs",
     "对从未发送过的草稿发起撤回。",
     "直接删除草稿即可；未发送的东西没有远端事实需要冲销。"),

    // ---- 幂等、并发与重放 --------------------------------------------------
    ("E_WRITEBACK_CONFLICT", External, true, "psychdoc-store/src/error.rs",
     "另一处正在写同一件（幂等键已被预占但未提交）。",
     "等对方提交完再重试；这是**唯一**为并发而设的可重试码。"),
    ("E_REPLAY_IN_DOUBT", Config, false, "psychdoc-store/src/error.rs",
     "上一次写回在「已外发未确认」时崩溃，重放请求被拒。",
     "人工核对院内系统是否已建件，再决定重发或销账；重试一万次也不会自己变好。"),
    ("E_CLAIM_UNKNOWN", Config, false, "psychdoc-store/src/error.rs",
     "幂等预占记录指向的目标已不存在。",
     "清理该预占后重新发起；目标不存在说明上一次流程已作废。"),
    ("E_CLAIM_STATE", Config, false, "psychdoc-store/src/error.rs",
     "预占记录处于不允许迁移的状态。",
     "按当前状态选择处置；预占四态的迁移都会留审计事件。"),

    // ---- 增量游标 ----------------------------------------------------------
    ("E_CURSOR_REWIND", Config, false, "psychdoc-store/src/error.rs",
     "增量读游标比上次回退（服务端数据被回滚或换了一套环境）。",
     "删掉该游标重跑一次全量读；回退后按旧游标增量会漏件。"),
    ("E_CURSOR_SHAPE", Config, false, "psychdoc-store/src/error.rs",
     "游标内容不是本产品写的形态。",
     "删掉游标重建。"),

    // ---- 队列与通知 --------------------------------------------------------
    ("E_QUEUE_EMPTY", Config, false, "psychdoc-store/src/error.rs",
     "队列里一条住院都没有，拒绝落库。",
     "先拖入或选择住院别名再标记已审；空队列会让重开后的左栏空着。"),
    ("E_QUEUE_NOTHING_SELECTED", Config, false, "psychdoc-store/src/error.rs",
     "队列里未勾选任何住院就标记已审。",
     "勾选要标记的住院别名。"),
    ("E_QUEUE_DUPLICATE_ENCOUNTER", Config, false, "psychdoc-store/src/error.rs",
     "同一住院别名在队列里出现两次。",
     "合并成一条再落库；重复别名会让左栏出现两行同一份住院。"),
    ("E_QUEUE_UNKNOWN_ENCOUNTER", Config, false, "psychdoc-store/src/error.rs",
     "标记的住院别名不在队列里。",
     "核对别名拼写，或先把它加入队列。"),
    ("E_NOTIFY_UNKNOWN", Config, false, "psychdoc-store/src/error.rs",
     "发件箱里没有这条待发通知（可能已被清掉或已发出）。",
     "重新生成缺件通知；已发出的通知不会重复发。"),

    // ---- 依赖图与归档核齐 --------------------------------------------------
    ("E_DEP_CYCLE", Config, false, "psychdoc-graph/src/lib.rs",
     "院内文书依赖规范成环（如约束记录 ↔ 解除记录互为前置）。",
     "改规则集里的依赖声明；本产品拒绝出图并把环内件交人工，不给一份永远「核不齐」的假结论。"),
    ("E_DEP_INVARIANT", Config, false, "psychdoc/src/contract.rs",
     "建图不变量被破坏（悬空边端点、自环、跨住院别名连边、节点 id 重复等）。",
     "核对依赖声明引用的文书类型是否在字典内、是否写错了住院别名。"),
    ("E_DEP_DANGLING", Config, false, "psychdoc-graph/src/lib.rs",
     "依赖边的端点不在节点表里（悬空引用）。",
     "核对依赖声明指向的文书类型；本产品记录而不丢弃这条边，视图上标为待人工。"),
    ("E_DEP_CROSS_ENCOUNTER", Config, false, "psychdoc-graph/src/lib.rs",
     "依赖边跨了住院别名（多半是把两份住院的件配到了一起）。",
     "核对住院别名；跨件时间序与归档核齐都以住院别名为键，串了会给出方向相反的结论。"),
    ("E_DEP_DUP_NODE", Config, false, "psychdoc-graph/src/lib.rs",
     "同一个节点 id 在图里出现了两次。",
     "核对文书 id；重复节点会让「这件卡住谁」数出两条一样的边。"),
    ("E_DEP_CYCLE_PROPAGATION", Config, false, "psychdoc-graph/src/lib.rs",
     "在成环图上请求阻塞传播闭包。",
     "先按 E_DEP_CYCLE 改掉环，再重算欠件天数。"),
    ("E_DIR_PAIRING", Config, false, "psychdoc-graph/src/lib.rs",
     "正反向对拍不一致（dependencies 与 dependents 画不出同一张可达集）。",
     "属建图不变量被破坏的信号；本产品宁可拒出视图也不给方向相反的两套答案。"),
    ("E_ENCOUNTER_UNKNOWN", Config, false, "psychdoc/src/graph_cmd.rs",
     "查的住院别名在本批文书包里不存在。",
     "核对别名；归档核齐视图以住院别名为键，别名错了整份视图就是空的。"),
    ("E_DOC_NOT_FOUND", Config, false, "psychdoc-ffi/src/api.rs",
     "要打开的件不在本次载入的包里。",
     "确认该件是否已被类型字典过滤或不属于本次载入目录；本产品报「未知件」而不是给空卡片。"),

    // ---- 外部对接：FHIR R4 -------------------------------------------------
    ("E_FHIR_UNREACHABLE", External, true, "psychdoc-fhir/src/error.rs",
     "EMR 端点连不上（DNS / 端口 / 超时）。",
     "确认 EMR_BASE_URL 与网络策略；退避后重跑。"),
    ("E_FHIR_STATUS", External, true, "psychdoc-fhir/src/error.rs",
     "端点返回 4xx / 5xx（401 令牌过期、404 路径写错、429 限流、5xx 服务端故障）。",
     "401 重新取令牌后重跑；404 核对 URL 路径；429 与 5xx 退避后重试。"),
    ("E_FHIR_SHAPE", External, true, "psychdoc-fhir/src/error.rs",
     "端点返回体不是预期的 FHIR R4 结构。",
     "核对端点版本；本产品不会把非 FHIR 正文当成空结果。"),
    ("E_FHIR_FIELD", External, true, "psychdoc-fhir/src/auth.rs",
     "响应缺必需字段（令牌缺 access_token、包缺 entry 等）。",
     "核对客户端凭据的 scope 与端点契约。"),
    ("E_FHIR_TXN_EMPTY", Config, false, "psychdoc-fhir/src/error.rs",
     "一次 transaction 没有任何 entry。",
     "先勾选要补写的条目；空 transaction 在**任何网络调用之前**就被拒。"),
    ("E_FHIR_TXN_REJECTED", External, true, "psychdoc-fhir/src/write.rs",
     "Bundle transaction 被端点部分或整体拒绝（返回 200 但 entry 带 4xx）。",
     "整批判失败、0 条部分写入，本地草稿保留；按端点返回的 entry 级原因修正后重发。"),
    ("E_FHIR_PAGING", External, true, "psychdoc-fhir/src/error.rs",
     "分页游标不前进（端点反复返回同一页）。",
     "核对端点分页实现；本产品不静默停在第一页。"),
    ("E_FHIR_RETRIES_EXHAUSTED", External, true, "psychdoc-fhir/src/error.rs",
     "429 / 5xx 退避重试次数用尽。",
     "拉长退避间隔或错开跑批时段后重跑。"),
    ("E_FHIR_READ", External, true, "psychdoc/src/contract.rs",
     "读文书包这一步整体失败（端点返回体、租户头或分页环节出问题）。",
     "退避后重跑；本产品不会把「读不到件」当成「全部通过」。"),
    ("E_FHIR_BIND", External, true, "psychdoc/src/contract.rs",
     "读回来的件绑定不唯一，无法确定按哪一份参与跨文书判定。",
     "人工在待确认队列里指定；绑定不唯一时整对跳过，不猜。"),

    // ---- 外部对接：企业微信缺件通知 ----------------------------------------
    ("E_WECOM_STATUS", External, true, "psychdoc-fhir/src/error.rs",
     "企业微信 Webhook 返回非 0 errcode（令牌失效、频率超限）。",
     "核对 WECOM_WEBHOOK_URL；通知失败只影响提醒，不影响审查结论。"),
    ("E_WECOM_REJECTED", External, true, "psychdoc-fhir/src/error.rs",
     "Webhook 返回 ok=false（消息体被拒）。",
     "核对消息体规范；同一住院同日重复发只留一条，不会刷屏。"),
    ("E_WECOM_SHAPE", External, true, "psychdoc-fhir/src/wecom.rs",
     "Webhook 返回体不是预期的 JSON 形态。",
     "核对 webhook 地址是否指向企业微信群机器人。"),

    // ---- 规则回归套件 ------------------------------------------------------
    ("E_REGRESS_SCHEMA", Config, false, "psychdoc-rules/src/regress.rs",
     "用例集 YAML 结构不合法（缺必填栏、kind 写错）。",
     "按 rulesets/rule-regress.yaml 的形状改用例文件。"),
    ("E_REGRESS_VERSION", Config, false, "psychdoc-rules/src/regress.rs",
     "用例集声明的版本与本程序支持的版本不符。",
     "改版本号或升级程序；用例集与执行器必须同版本，否则期望值不可信。"),
    ("E_REGRESS_EMPTY", Config, false, "psychdoc-rules/src/regress.rs",
     "用例集一条用例都没有。",
     "补用例；空用例集会「全绿」成假通过，覆盖矩阵为 0 也会被报出来。"),
    ("E_REGRESS_NO_ID", Config, false, "psychdoc-rules/src/regress.rs",
     "用例没有 id。",
     "给每条用例一个稳定 id，便于按用例定位失败。"),
    ("E_REGRESS_DUP_ID", Config, false, "psychdoc-rules/src/regress.rs",
     "用例集里出现重复 id。",
     "改掉重复 id；重名会让「哪条用例失败」无法回答。"),
    ("E_REGRESS_CODE_SHAPE", Config, false, "psychdoc-rules/src/regress.rs",
     "用例里的规则码不是 QC-PSY-nn 形态。",
     "核对规则集里的码与用例期望里的码是否同一套。"),
    ("E_REGRESS_NO_FIXTURE", Config, false, "psychdoc-rules/src/regress.rs",
     "用例没有 given.fixture（本轮按用例驱动回归，无默认夹具）。",
     "给用例指定夹具文件名。"),
    ("E_REGRESS_POSITIVE_NO_CODE", Config, false, "psychdoc-rules/src/regress.rs",
     "正例用例跑完没命中期望的码。",
     "先看是该改规则集还是该改用例；本产品不自动改期望值。"),
    ("E_REGRESS_NEGATIVE_WITH_CODE", Config, false, "psychdoc-rules/src/regress.rs",
     "反例（合规件）跑出了命中——误报守卫拦下了。",
     "收窄规则的适用范围或修实现；误报比漏报更伤——没人再敢信这份清单。"),
    ("E_REGRESS_FIXTURE_MISSING", Config, false, "psychdoc/src/main.rs",
     "用例点名的夹具文件在磁盘上找不到。",
     "确认 --root 指向项目根，或夹具是否被清理阶段删掉。"),
    ("E_REGRESS_FIXTURE_UNPARSABLE", Config, false, "psychdoc/src/main.rs",
     "夹具文件读得出来但解析不了（不是合法 Bundle）。",
     "确认夹具没被改坏；脏数据用例请用夹具目录里的 dirty/ 那一批。"),
}

/// 前缀族：回归套件的码随用例类别增长，逐个登记会让表尾拖一长串。
/// 本产品在 [`lookup`] 里先按前缀匹配这一族，其余走全表。
pub const REGRESS_PREFIX: &str = "E_REGRESS_";

/// 查一个码。查不到返回 `None`——**不猜、不归并到某个近似的码**。
///
/// 前缀族（`E_REGRESS_`）命中时回 `E_REGRESS_SCHEMA` 那一条的形状，
/// 因为它们共用同一套「改用例集而不是改规则集」的处置方向。
pub fn lookup(code: &str) -> Option<&'static ErrorCode> {
    if let Some(found) = ERROR_CODES.iter().find(|c| c.code == code) {
        return Some(found);
    }
    if code.starts_with(REGRESS_PREFIX) {
        return ERROR_CODES
            .iter()
            .find(|c| c.code == "E_REGRESS_SCHEMA")
            .filter(|_| code.len() > REGRESS_PREFIX.len());
    }
    None
}

/// 处置侧。查不到的码归 [`Side::Config`]：把未知故障报成网络问题会引发
/// 重试风暴，报成配置问题最多让运维多看一眼。
pub fn side_of(code: &str) -> Side {
    lookup(code).map(|c| c.side).unwrap_or(Side::Config)
}

/// 是否值得稍后重试。查不到即 `false`。
pub fn is_retryable(code: &str) -> bool {
    lookup(code).map(|c| c.retryable).unwrap_or(false)
}

/// 一行中文说明，可直接贴进工单：`E_XXX（配置 / 用法 · 不可重试）：触发条件 → 处置建议`。
pub fn describe(code: &str) -> String {
    match lookup(code) {
        Some(c) => format!(
            "{}（{} · {}）：{} → {}",
            c.code,
            c.side.as_zh(),
            if c.retryable { "可重试" } else { "不可重试" },
            c.trigger,
            c.remedy
        ),
        None => format!(
            "{code}（未登记的错误码）：本版本没有它的说明，按「配置 / 用法 · 不可重试」处理并报信息科"
        ),
    }
}

/// 导出为 markdown 表格片段，供 README「错误与降级」一节直接引用。
///
/// 表头刻意多一列「触发条件」：只写「不可重试」的表没有用——
/// 值班的人要判断的是**这次是不是同一件事**。
pub fn markdown_table() -> String {
    let mut out = String::from(
        "| 错误码 | 处置侧 | 可重试 | 触发条件 | 处置建议 |\n\
         |---|---|---|---|---|\n",
    );
    for c in ERROR_CODES {
        out.push_str(&format!(
            "| `{}` | {} | {} | {} | {} |\n",
            c.code,
            c.side.as_zh(),
            if c.retryable { "是" } else { "否" },
            c.trigger,
            c.remedy,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn the_table_has_at_least_ten_codes_and_no_duplicates() {
        assert!(ERROR_CODES.len() >= 10, "错误码表只有 {} 条", ERROR_CODES.len());
        let mut seen = BTreeSet::new();
        for c in ERROR_CODES {
            assert!(seen.insert(c.code), "错误码 {} 重复登记", c.code);
        }
    }

    #[test]
    fn every_row_carries_a_trigger_and_a_remedy() {
        for c in ERROR_CODES {
            assert!(c.trigger.len() >= 6, "{} 的触发条件太短", c.code);
            assert!(c.remedy.len() >= 6, "{} 的处置建议太短", c.code);
            assert!(!c.origin.is_empty(), "{} 没写返回位置", c.code);
            assert!(c.origin.starts_with("psychdoc"), "{} 的 origin 不是 crate 路径", c.code);
        }
    }

    #[test]
    fn codes_all_follow_the_stable_shape() {
        for c in ERROR_CODES {
            assert!(c.code.starts_with("E_"), "{} 不是 E_ 前缀", c.code);
            let tail = &c.code[2..];
            assert!(
                tail.chars()
                    .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_'),
                "{} 含非大写字符",
                c.code
            );
        }
    }

    #[test]
    fn an_unknown_code_is_never_guessed_into_a_neighbour() {
        assert!(lookup("E_NOT_A_REAL_CODE").is_none());
        // 未登记 → 配置侧 + 不可重试：宁可让运维多看一眼，也不引发重试风暴。
        assert_eq!(side_of("E_NOT_A_REAL_CODE"), Side::Config);
        assert!(!is_retryable("E_NOT_A_REAL_CODE"));
        assert!(describe("E_NOT_A_REAL_CODE").contains("未登记"));
    }

    #[test]
    fn the_regress_family_resolves_by_prefix() {
        assert!(lookup("E_REGRESS_DUP_ID").is_some());
        // 前缀族里任意新码也能查到处置方向——否则每次加用例类别都要改表。
        assert!(lookup("E_REGRESS_SOMETHING_NEW").is_some());
        // 光有前缀没有后缀不算命中。
        assert!(lookup("E_REGRESS_").is_none());
    }

    #[test]
    fn the_markdown_table_has_one_row_per_code_and_names_every_code() {
        let md = markdown_table();
        for c in ERROR_CODES {
            assert!(md.contains(c.code), "markdown 表缺 {}：\n{md}", c.code);
        }
        let rows = md.lines().filter(|l| l.starts_with("| `E_")).count();
        assert_eq!(rows, ERROR_CODES.len(), "markdown 行数与表长度对不上");
    }

    #[test]
    fn retryable_codes_are_all_on_the_external_side() {
        // 可重试意味着「环境会自己变好」；配置侧的错误不会自己变好。
        // 一旦出现 Config + 可重试的行，就是分类写错了。
        for c in ERROR_CODES {
            if c.retryable {
                assert_eq!(
                    c.side,
                    Side::External,
                    "{} 标成可重试却归在配置侧——CI 会陷入无意义的重试",
                    c.code
                );
            }
        }
    }
}
