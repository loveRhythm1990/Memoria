# cuVS / CAGRA 本地实验分支

本分支只用于实验，不承诺合并到 main。Memoria 源码保留本地，不 push、不创建源码 PR。按后续授权，将固定本地提交构建为 linux/amd64 镜像并推送上海 IDC 仓库，部署配置通过 moi-gitops 的 draft PR 审查；不直接修改远程集群。

## 本轮实现

- `MEMORIA_CUVS_EXPERIMENT=true`：新建的用户/群组记忆主表使用 `id BIGINT AUTO_INCREMENT PRIMARY KEY`，保留 `memory_id VARCHAR(64) NOT NULL UNIQUE` 作为业务标识。
- 检查已有主表的真实列元数据。旧字符串主键、不符合要求的整数键或缺少业务 ID 唯一约束时拒绝初始化，不转换旧表；检查在 schema version 快速返回之前执行。
- 实验标记随用户/群组路由和后台连接池传递。
- 实验模式不启动 IVFFlat 监控/重建 worker，存储层的 IVF 重建及重建条件检查也直接跳过，覆盖后台治理、插件及直接调用。
- `MEMORIA_GRAPH_RETRIEVAL_ENABLED=false`：明确跳过关系图检索，也不创建专用图查询连接池。cuVS 实验默认关闭且不允许开启图检索。
- 向量、全文融合及用户/session/subject/type 过滤保持现有逻辑；本轮没有重写排序算法。
- API serve 与本地 MCP 的启动入口均接入配置。

未启用实验模式时，主表及 IVFFlat 管理继续使用原有行为。图检索默认开启，也可以独立关闭。

## 实验配置

```text
MEMORIA_CUVS_EXPERIMENT=true
MEMORIA_MULTI_DB=true
MEMORIA_GRAPH_RETRIEVAL_ENABLED=false
ENTITY_POOL_SIZE=0
```

数据库连接和 embedding 配置另外通过环境提供，不把凭据写入本文件。必须使用专用全新数据库；`MEMORIA_SHARED_DATABASE_URL` 应指向实验专用的共享注册库，各测试用户由 router 创建自己的数据库。

cuVS 模式要求显式 `MEMORIA_MULTI_DB=true`，在自动拓扑检测/旧库迁移前检查配置，避免实验入口触发自动迁移。两个新开关的非法布尔值会在运行时启动校验中报错。

`ENTITY_POOL_SIZE=0` 是已有配置，用于关闭异步实体提取，减少实验干扰；它不会关闭所有图数据写入。本轮关闭的是关系图**检索**，并未删除图表或全面重构图写入。

第一轮只验收默认 main 记忆表。分支、快照恢复、合并、历史表和其他高级操作不在当前验收范围，不应在实验中使用。新增内部整数 ID 后，这些操作的列复制、主键语义和索引重建还需要单独验证。

## 当前如何管理 CAGRA

Memoria 本轮不自动创建 CAGRA，不自动设置 MO 实验变量，也不代替 MO 管理增量维护。由测试操作者在**实际测试用户的数据库**中，导入数据后显式运行 [建索引脚本](../scripts/cuvs/create_cagra_index.sql)。每个用户/群组数据库都需要分别处理，不能只在共享注册库中建索引。

建议顺序：

1. GPU MO 纯 SQL 建索引、搜索、增量更新验证通过。
2. 启动使用上述配置的专用 Memoria，写入合成记忆。
3. 确认该用户数据库和主表结构，显式创建 CAGRA。
4. 保存 Memoria 真实检索 SQL 的 `EXPLAIN`，确认命中 CAGRA。
5. 验证召回、隔离、过滤、更新与删除，并记录增量可见延迟。

如果没有建索引，现有 SQL 仍可能通过扫描和全文融合返回记忆。`retrieve_explain` 的 `vector_hit=true` 只证明向量候选非空，**不能证明 CAGRA 被使用**。必须另外保存数据库查询计划。

## 本地验证

单元测试不需要 GPU 或数据库：

```bash
rtk proxy cargo test --locked -p memoria-storage -p memoria-service --lib
```

从仓库的 `memoria/` 目录执行。先确保测试进程没有继承指向远程环境的数据库、LLM 或 embedding 配置。

新增 CPU MO 集成测试默认忽略。只有显式设置 `MEMORIA_CUVS_LOCAL_DATABASE_URL` 并加 `--ignored` 才执行；只接受 `127.0.0.1` 或 `::1` 字面地址，拒绝其他主机。它不读取通用 `DATABASE_URL`。

```bash
# 先把专用本地 MO 的连接放入 MEMORIA_CUVS_LOCAL_DATABASE_URL；不要提交凭据。
rtk proxy cargo test --locked -p memoria-service --test cuvs_local_it \
  -- --ignored --test-threads=1
```

测试会创建随机命名的共享测试库、测试用户和用户库，成功后删除本次创建的库。失败时可能保留数据用于排查，因此只能针对专用本地 MO 运行，不能映射到共享环境的数据库端口。

覆盖范围：

- 按用户创建整数主键表、重复迁移和 schema version 快速路径。
- 字符串业务 ID 保持稳定；重复请求不重复写入；不同记忆取得不同整数 ID。
- 用户数据库隔离、跨会话召回及 strict session 过滤。
- 普通无额外过滤的查询确实跳过图检索，并实际取得向量候选。
- 向量修改、软失效、物理删除，以及后台 store 的实验策略继承。
- 旧表拒绝实验模式且不转换主键。

CPU MO 测试不能证明 CAGRA、CUDA、GPU 显存或索引 CDC 行为正确。这些仍等待单 CN、单 GPU 的专用 MO 环境验证。

## 后续环境

QA 中准备专用 Memoria（本实验分支镜像）和独立 GPU CNGroup，包含一个 CN 和一张 GPU，复用现有 QA 父集群的 DN、LogService、Proxy 与对象存储。测试使用用户另建的专用租户，不接管现有 Memoria 流量。Memoria 直接连接专用 GPU CN 的 SQL Service，仍需分别验证查询、索引构建及后台维护的执行位置。租户连接信息补齐前，专用 Memoria 保持零副本；部署通过用户 review 后的既有 GitOps 发布流程进行。
