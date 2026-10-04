# AML 文本接入：同事 Review 说明

整理日期：2026-10-04。范围为同事的参考原型、Memoria AML 适配、本地接口与公共数据评测程序。本文描述可审查的当前实现，不是已通过独立代码审查或正式参评验收的结论。

方案与设计决策统一维护在 [AML 文本记忆接入：方案与设计](aml-text-adapter.md)。本文用于审查范围和交付物说明；架构、时间语义、来源生命周期及优化顺序以设计文档为准。

## 当前是否完善

当前版本可以交给同事 review：核心 Add/Search、MatrixOne 持久化、幂等、用户隔离已实现，公共 LoCoMo 写入、检索、回答与评分流程已跑通。当前含来源上下文补全的固定 150 题已完成；最新完整回归套件尚未重跑。

**尚不能作为已完成正式参评准备的版本。** 物理清理、部署容量验证、正式模型配置核对及官方 Smoke 尚待完成。检索质量优化需根据评测定位，不应把参考原型的全部算法自动视为必需迁移项。

## 三部分工作及来源

| 部分 | 位置 | 职责与来源 |
|---|---|---|
| 同事原型 | `~/Downloads/aml-memory-endpoint` | 原同事的独立 FastAPI + SQLite/FTS5 Add/Search；未接入 Memoria |
| Memoria 适配 | `~/runtime-agent/memoria-aml` | 本次修改，Memoria Rust API 内接入 AML；不另部署 SQLite 服务 |
| 本地评测客户端 | `~/runtime-agent/aml-local-client` | 本次新建，模拟请求及公共 LoCoMo 小集；不是 Memoria 核心的一部分 |

同事原交接整理当前文件名为 `交接总结 2.md`。原型 README 与代码是 review 输入，README 中的 35/35、性能数字不是本次重新测得，也不能直接代表 Memoria。

## Memoria 版本与审查范围

- Worktree：`/Users/lr90/runtime-agent/memoria-aml`。
- 分支：`feat/aml-text-adapter`。
- 实现及现有运行证据基线：`689f3f9badf0ae1eba3652ab91a8a329dc08701b`，v0.5.2；PR 分支已同步目标 `origin/main` 的 `e180dff`。
- GitHub 审查仓库：`loveRhythm1990/Memoria`（本机 remote 名为 `origin`）；审查分支为 `feat/aml-text-adapter`，PR 目标为该仓库的 `main`。代码作为分支提交交付，尚未合并；审查应注明具体 commit。
- 目标 main 比运行基线新增 4 个提交，已合入审查分支。`store.rs` 候选合并冲突已整合为 main 的合并函数与 AML 独立向量分数映射；已有运行证据不覆盖同步后的完整版本，同步后 API／CLI `cargo check` 通过；评测及完整回归尚未重跑。

建议按下面顺序阅读：

| 文件 | 审查内容 |
|---|---|
| `memoria/crates/memoria-api/src/routes/aml.rs` | 契约、认证、ID 映射、文本分块、来源信息、响应 |
| `memoria/crates/memoria-api/src/auth.rs` | `/aml/` 绕过普通 actor scope 后是否仍由专用认证保护 |
| `memoria/crates/memoria-api/src/state.rs`、`lib.rs`、`routes/mod.rs` | 配置开关、路由注册 |
| `memoria/crates/memoria-service/src/service.rs` | `ingest_source_batch` 的来源事件写入路径、隐私策略、embedding、实体任务 |
| `memoria/crates/memoria-core/src/source.rs`、`types.rs` | 来源位置／链接结构、来源置信度年龄策略与检索选项 |
| `memoria/crates/memoria-service/src/source_context.rs` | 邻接定位、范围校验、有界候选与预算、输出顺序、Explain 与回退 |
| `memoria/crates/memoria-storage/src/graph/retriever.rs`、`graph/store.rs` | 图路径来源策略查询、降级与有界主键读取 |
| `memoria/crates/memoria-storage/src/store.rs` | 来源请求表、幂等查询、事务、并发重试、通用 insert 重构 |
| `memoria/crates/memoria-api/tests/aml_adapter.rs` | 真实 MatrixOne/HTTP 测试的断言与边界 |
| `.env.example`、`docker-compose.yml`、`docs/aml-text-adapter.md` | 部署与公开容量说明 |

## 交接原型的功能对应关系

| 原型功能或建议 | Memoria 处理 | 审查结论应关注 |
|---|---|---|
| Health、Add/Search，证据而非答案 | 已实现 `/health`、`/aml/add`、`/aml/search` | 服务地址可自定义，协议固定 |
| SQLite/FTS5 持久化与检索 | 替换为 Memoria/MatrixOne | 没有迁移 FTS5 SQL；检索行为不保证相同 |
| user_id 隔离、跨会话查询 | 稳定哈希范围，固定 main，Search 不使用 session_id | 长/特殊标识、跨用户、密钥轮换与多数据库 |
| `(user_id, session_id, request_id)` 幂等 | 持久请求凭据，载荷哈希，事务写入 | 并发、重启、不同内容冲突、失败后重试 |
| 1000 字符、150 重叠分块 | 使用 Unicode 字符切分 | 保留原文；未迁移段落/句子边界优先切分 |
| 角色、时间、来源 | 前缀与 metadata 保留 | 时间语义、消息顺序、来源定位 |
| CJK 双字、trigram、RRF、相邻分块加权 | 未迁移原型算法，复用 Memoria 检索 | 根据失败例决定是否在核心检索层改进 |
| 可选 embedding 候选重排 | 使用 Memoria 独立向量检索能力 | 并非原型排序的等价移植 |
| 调试 stats/purge | 未迁移公开调试路由 | 不将原型未鉴权 purge 放到公网 |
| 按用户物理清理 | 未补齐 | 现有软删除不能满足完整物理清理 |
| 多种认证、多模态 | 当前专用 Bearer、仅文本 | 与本次文本赛道范围一致；非全功能移植 |
| 35 项 smoke、48 Add/96 Search 压测 | 未完整移植原脚本或复测其性能数字 | 本次集成测试与负载压测不是同一个结论 |
| 七能力本地评测集 | 未完成 | 现有 core-v1 早已存在；LoCoMo 小集不等于七维覆盖 |

## 当前关键设计

1. 专用 `MEMORIA_AML_API_KEY`，未配置时不注册 AML 路由；与管理 Key、普通用户 API Key 分离。Health 不认证。
2. Add 同步执行：隐私策略处理、embedding（如配置）、记忆和请求凭据事务提交，然后返回成功。不同来源事件不走普通事实注入的语义去重/覆盖。
3. 幂等以完整外部用户、会话、请求组合映射；同一组合更换语义载荷返回 409。已有成功请求在生成 embedding 前识别。
4. Search 复用 `retrieve_with_options_on_branch(..., main, ...)`。query 原样，不拼入 options，不生成答案，不静默截断证据。接口允许 top_k 1—1000，正式评测使用 100。
5. 来源消息时间保留在正文及 metadata；AML 来源记录 `observed_at` 使用接收时间，与初次 `created_at` 一致。来源写入服务设置通用 `source_evidence` 标记，核心、混合与图检索不对其按年龄衰减；内置年龄清理及语义冗余删除排除来源记录。普通记忆默认年龄策略保持原状。旧 run 不自动补标记，重放不刷新时间；完整保留期限清理待实现，详见设计文档。
6. 实体抽取入队仍为后台任务。Add 成功保证来源文本和向量（如配置）已提交，**不保证整张实体图已构建完成**，需要审查立即 Search 与图谱路径的关系。
7. 继承 2 MiB 请求体上限，超限返回 413；正式容量应申报并测量。
8. 当前隐私规则：HIGH 内容拒绝，MEDIUM 内容脱敏。应评估是否会影响合法评测样本、证据完整性和样本完成率；不能静默绕过失败后宣称同一基线。

## 已有验证证据

| 验证 | 当前证据 | 限制 |
|---|---|---|
| 本轮排序与来源策略编译／部署 | API/CLI `cargo check`、Docker release 构建通过；API 已重建并重启，健康接口 HTTP 200 | 两组质量小集已完成；最新优化的单元／独立数据库集成套件尚未重跑 |
| 批次内来源上下文补全 | 已通过编译和 release 部署；本地 ±2／5／20，实际主键读、幂等及用户隔离检查通过；四个目标回复确认被补齐 | 默认仍关闭；完整分支／停用／治理边界套件未重跑，旧数据不补链接；前位排序、时间题与时延代价见统一设计文档 |
| API 单元测试 | 实现时 103 项通过 | 本文整理未重新运行 |
| Clippy | 实现时相关 API/lib/test 范围通过 `-D warnings` | 本文整理未重新运行 |
| 真实 MatrixOne + HTTP 集成测试 | 实现时 3 项通过 | 默认 ignore，需显式数据库；不是 3 个简单接口用例 |
| 本地真实模型接口演示 | 两个会话、重放 Add、四题 Search、用户隔离通过 | 小数据接口演示，不是官方 Smoke |
| LoCoMo 公共小集 | 无补全基线镜像完成两组各 28 Add、20 Search/Answer，全为 HTTP 200 | 一个对话、19 会话、419 消息、前 20 道文本题；每组一次 |
| 词面评分与证据覆盖 | 当前客户端 F1 0.3716、BLEU-1 0.2713、Recall@100 70.8%；旧内容对照 0.3630／0.2729／62.5%；旧基线 0.2964／0.2096／62.5% | 分数与覆盖越大越好；不是正确率或榜单分数，方法和逐题限制见设计文档 |
| 来源检索诊断 | 两个新 run 各抽查 2 题：hybrid、来源标记、时间分数 0、置信度 0.95、正向量分数均确认 | 图候选为 0，不覆盖图策略和治理生命周期 |
| 严格 judge 小集 | Qwen3-14B 三组现有预测已评分，旧基线 65%，当前两组均 75% | 同一非随机 20 题；已发现日期回答假阳性，不代表人工正确率或产品排名 |
| 跨对话 / 全量 / 官方 Smoke | 同一固定 150 题，无补全 118/150（78.67%）→补全 127/150（84.67%）；13 改善／4 退化，Recall@100 82.16%→90.27% | 时间题 55.56%→48.15%，Search P95 511→835 ms，尚未定版；50 题验证、全集及官方均未运行，不等于人工正确率或榜单成绩 |

集成测试覆盖认证/关闭、输入校验、请求回显、用户隔离、跨会话、重放/内容冲突/并发、事务回滚、embedding 失败恢复、来源事件不被语义去重、隐私和 413 边界。复跑方式见 `docs/aml-text-adapter.md`，应使用独立测试数据库。

本地模型：embedding 使用 Shanghai 的 BAAI/bge-m3（1024 维）；Memoria 内部 LLM 和本地回答分别配置为 DashScope qwen-plus。原 SiliconFlow Key 返回余额不足，已替换。**模型密钥不在 review 包中。**

## Review 重点与未完成事项

### 合并前优先审查

- 来源批量写入是否破坏已有 store/分支/治理语义，通用 insert 重构是否影响原有 NULL/向量参数行为。
- 请求凭据表在单/多数据库及旧库升级时的创建、并发冲突恢复、事务提交与错误分类。
- 新路由的认证边界与 actor scope 例外；有没有跨租户读写路径。
- 源消息是否完整保留，角色/时间/分块映射是否足以用于时间题、多跳题。
- 来源上下文新实现：`memoria-core/src/source.rs` 与 `memoria-service/src/source_context.rs` 的批次边界、链接上限、保护锚点、预算及顺序；storage 主键批量读是否限定用户／分支／active，当前路由数据库的限定表名校验、降级和 Explain 是否明确；保护锚点记录与保持其前位名次需分别审查。普通检索默认关闭；旧数据不自动补链接。
- 后台实体任务的失败或丢失、立即 Search 的降级行为和可观测性。
- 隐私处理对合法样本的影响，幂等载荷比较与脱敏之间的一致性。
- 请求/响应错误是否明确，客户端能否保持相同 request_id 与载荷进行重试。

### 正式参评前必须补齐或确认

1. **完整物理清理流程**：记忆、来源 metadata、实体/派生数据、请求凭据、实际日志/备份范围。多数据库还需清理用户数据库和共享登记。当前只有部署责任说明，没有自动清理任务或完成证明。
2. **模型配置与组别规则**：公开规范对开源方法 Add 预期使用 gpt-4o-mini；当前 qwen-plus 是本地开发配置，应核对 Add 实体抽取等使用范围，不可原样宣称符合正式开源提交规则。
3. 公网接口、HTTPS/网关、认证提交和健康检查，以及选定并发下的写入、检索、连接池、外部模型限流与容量验证。
4. 固定提交版本，完成官方 Smoke，然后再申请/运行正式评测。当前均未完成。

上述模型及数据规则来源为 [AML API Guide](https://agentmemories.ai/api-guide)，2026-10-04 核对：正式数据只用于本次任务，默认在任务完成后 30 天内删除；开源与商业组别的模型限制不同。

### 质量调优阶段

- 已有 20 题完成 Qwen3-14B refined judge；结合人工复核区分召回、回答与裁判问题。
- 跨对话固定抽样、调度和汇总已实现；冻结 150 题开发／50 题保留验证，完整运行与质量结论以设计文档为准。
- 本阶段限定 AML 文本参评：先建立只走 Add/Search 的 AML 能力小集，再做固定 LoCoMo 的输出排序对照，补全预算另行实验；不扩展为全产品评测。50 题保留集与 861 题全集只验证 LoCoMo 范围。具体工作及官方契约核对见设计文档“AML 文本参评：下一步执行顺序”。
- 七能力覆盖单独规划，不把公共 LoCoMo 或原有 core-v1 当作完整官方套件。

## 本地客户端审查路径

先读 `README.md`、`docs/API.md`、`docs/LOCOMO.md`，再看：

- `main.py`：根目录薄入口。
- `aml_local_client/protocol.py`：发送侧文本契约校验；额外字段限制是本地策略。
- `aml_local_client/client.py`：明确 Add/Search、超时、有限重试、回显/响应校验、脱敏展示。
- `aml_local_client/locomo.py`：完整对话写入、文本题筛选、角色/时间本地映射、断点、回答、上游评分。
- `aml_local_client/suite.py`：冻结跨对话开发／验证题目、来源哈希检查、顺序调度、评分复用与分项／证据／时延汇总。

特别检查金标隔离：Add 只使用原始 conversation，Search 只使用原题，Answer 只收到问题与召回证据；标准答案和 evidence 标注仅用于本地评分/分析。角色映射、UTC 假设、空白计词及 qwen-plus 回答提示词均为本地适配，不声称与 AML 冻结流程相同。

## Review 交付物与边界

本地审查快照：`/Users/lr90/runtime-agent/aml-review-package-20261004-191613`。包含审查说明、统一设计文档、完整 Memoria 补丁、同事原型代码快照、当前本地客户端代码快照、汇总结果与文件哈希清单。补丁包含来源上下文补全、来源年龄策略及限定表名校验修正。

旧目录 `~/runtime-agent/aml-review-package` 不是当前实现快照，请使用上述本地快照，GitHub 审查以分支 commit 为准。快照不包含私有 `.env`、模型 Key、数据库、评测正文／预测日志或上游数据集；仅包含密钥占位用的 `.env.example`。个人本机 `AGENTS.md` 不作为产品补丁交付。上游来源与固定 commit 在客户端文档中保留；同事原型使用已有原始审查快照，其 README 性能声明未重新测量。

补丁以指定基线生成，包含新增文件；在干净的同版本 checkout 中先执行 `git apply --check memoria.patch`，确认后再 apply。不要在已有本次修改的 worktree 再次 apply。本地包记录提交前的快照，包含的“未提交”状态仅描述打包时刻。当前 GitHub 审查以分支及 PR 的具体 commit 为准；使用本地包时注明基线和补丁 SHA256。
