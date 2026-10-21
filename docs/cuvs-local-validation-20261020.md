# cuVS 实验分支：本地实现与验证记录

日期：2026-10-20（Asia/Shanghai）

## 代码位置与交付边界

- Worktree：`~/runtime-agent/memoria-worktrees/cuvs-cagra`
- 本地分支：`experiment/cuvs-cagra`
- 起点：本轮读取的上游 main，`0695ad61eec9104478f05f5824b75bdbeeca5616`。
- 改动保留为该 worktree 中的本地文件修改及新增文件，尚未创建本地提交；没有 push 或创建 PR。
- 原 Memoria 工作目录及其当前开发分支保持不变。本轮没有部署远程服务或修改共享集群。

配置与操作说明见 [cuvs-experiment.md](cuvs-experiment.md)。

## 本轮实现

| 内容 | 实现情况 |
| --- | --- |
| 实验开关 | `MEMORIA_CUVS_EXPERIMENT=true`，默认关闭 |
| 整数内部主键 | 新用户/群组主表采用有符号 BIGINT 自增主键，保留字符串业务 ID 唯一约束 |
| 旧表检查 | 在兼容迁移和 schema version 快速返回前检查实际主键列；不转换旧表 |
| 用户路由与后台连接池 | 继承实验策略；后台 store 保留数据库限定名 |
| IVFFlat 管理隔离 | 不启动实验实例的 IVF monitor/worker；存储重建入口直接跳过，避免治理或插件绕过 |
| 关系图检索 | 独立开关；实验模式默认且强制关闭，不创建图查询专用池 |
| 启动入口 | API serve 与本地 MCP 接入上述配置；实验要求显式多库配置，拒绝隐式旧库迁移 |
| CAGRA 创建 | 提供显式 SQL 脚本，尚未在 GPU MO 执行；不会由 Memoria 自动创建 |

没有更改向量距离、全文融合权重或业务过滤条件。关闭关系图检索不等于删除图表或完全关闭图写入。

## 测试环境

- 宿主架构：ARM64。
- Rust / Cargo：1.96.0。
- 数据库：本轮新建的专用本地 Docker CPU MatrixOne，报告版本 `8.0.30-MatrixOne-v4.2.5`。
- CPU 镜像：`matrixorigin/matrixone@sha256:4455f6904a0cca5bb0f876d3026c407755216f91a4413a623c028495e0bba20f`。
- 容器限制为 2 CPU、2 GiB 内存，仅在 loopback 暴露数据库端口，未挂载既有业务数据目录。
- 测试使用合成记忆与固定 4 维向量，不调用真实 LLM 或 embedding 服务。
- 执行前清除继承的数据库、LLM、embedding 及 Memoria 环境配置；数据库集成测试只注入本次专用本地连接。
- 为节省本机磁盘，构建复用已有 Cargo target 缓存，限制为 2 个编译 job，并关闭增量编译。

完成测试后，已回收本次创建的专用数据库容器及其临时存储。

## 检查结果

以下 Cargo 命令从 worktree 的 `memoria/` 目录执行；实际执行时另行设置本机缓存路径及上述隔离环境，不在记录中保存连接凭据。

| 检查 | 结果 |
| --- | --- |
| `cargo check --locked --workspace --tests` | 通过，包含各 crate 的测试目标编译 |
| `cargo test --locked -p memoria-service --lib` | 71 项通过 |
| `cargo test --locked -p memoria-storage --lib` | 68 项通过；1 项既有的专用数据库 branch probe 测试按其默认标记忽略 |
| `cargo test --locked -p memoria-cli --bin memoria` | 28 项通过 |
| `cargo test --locked -p memoria-service --test cuvs_local_it -- --ignored --test-threads=1` | 2 项通过，实际连接本轮专用本地 MO |
| 新增 Rust 文件格式检查 | 通过 |
| `git diff --check` | 通过 |

合计 **169 项测试通过**，另有上述 1 项既有忽略测试未执行。没有执行全仓库所有数据库/外部服务测试。

## 数据库集成测试实际验证的内容

第一项测试覆盖：

1. 创建独立共享库及两个合成用户的数据库，主表实际使用自增整数 ID。
2. 重复迁移，包括 schema version 已为当前版本时的校验路径。
3. 同一业务写入请求重复执行得到同一字符串业务 ID，表中不重复插入。
4. 不同记忆生成不同整数 ID，不同用户数据库互相隔离。
5. 无额外过滤的普通检索未尝试关系图；实际取得向量候选，并返回当前混合检索结果。
6. 跨会话检索和 strict session 过滤。
7. 修改向量后，向量近邻查询反映新向量；软失效和物理删除后不再召回对应记忆。
8. 后台连接池继承实验策略与数据库限定名；调用 IVF 重建入口不会创建 IVF 索引。

第二项测试覆盖：默认模式新建的字符串主键表，在启用实验模式后被拒绝初始化；未被添加整数主键或自动转换。

新增无数据库单元测试还检查了：不合格主键和业务 ID 约束被拒绝、路由 store 在不可达连接上跳过 IVF 操作、实验服务不建立图查询池和 IVF monitor，以及 CLI 在连接数据库前拒绝不合格实验配置。

## 结论与待验证范围

本地证据支持：**Memoria 的实验表结构、关系图检索隔离、IVFFlat 管理隔离以及核心存储/检索流程已实现并通过 CPU MO 测试。**

以下仍未验证，不能视为通过：

- GPU MO 镜像的 CUDA/cuVS 运行依赖、设备可见性及后台任务位置。
- CAGRA 的首次构建、加载、真实 SQL 索引命中和 GPU 资源占用。
- 索引创建后的 CDC 插入/更新/删除可见延迟与重建恢复。
- CAGRA 下的过滤召回率、返回不足 k、IVFFlat 对比及性能收益。
- 实际 HTTP API 网络入口、真实模型提取/embedding 和 Agent 集成。
- 分支、快照恢复、合并、历史表等高级操作；这些不属于当前首轮验收范围。

本轮 CPU 集成测试的 `vector_hit=true` 只证明向量检索取得候选，不能证明使用 CAGRA。下一步需要专用单 CN、单 GPU MO，先执行纯 SQL 验证，再按测试用户数据库显式创建 CAGRA 并保存 Memoria 真实查询的执行计划。
