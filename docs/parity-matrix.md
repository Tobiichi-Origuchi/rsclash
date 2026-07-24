# CVR 功能对齐矩阵

本矩阵记录 rsclash 对 Clash Verge Rev（CVR）用户可见能力的迁移结果。`完成`表示已接入
原生 Rust/egui 路径并有自动化验证；`后续阶段`表示边界已确定但由后续里程碑负责；`不采用`
表示经过设计决策有意使用不同语义。

## Profiles（P7）

| CVR 能力 | rsclash 原生实现 | 状态 | 验证或边界 |
| --- | --- | --- | --- |
| 本地 YAML 导入 | 普通文件与 symlink/大小检查后事务式复制 | 完成 | profile lifecycle 测试 |
| HTTP(S) 订阅导入 | 有界下载、重定向限制、独立超时与 User-Agent | 完成 | 本地 HTTP fixture |
| 剪贴板导入 | egui/winit 原生 paste 请求填入统一 URL 输入 | 完成 | 不引入 GTK 剪贴板 |
| 拖放文件导入 | YAML 作为本地配置，PNG/JPEG 作为二维码 | 完成 | 统一进入 app command |
| 二维码导入与分享 | `rqrr` 解码、`qrcode` 生成内存 module matrix | 完成 | PNG 往返与快照脱敏测试 |
| `clash://`/`clash-verge://` 深链 | app worker 统一解析，支持 percent-encoding | 完成 | parser 单元测试 |
| 桌面 scheme 注册与第二实例转发 | 平台 adapter 与单实例通道 | 后续阶段（P9） | 当前命令行参数已进入同一导入队列 |
| 重命名、复制、排序、单项/批量删除 | 串行 worker 和原子 catalog/profile transaction | 完成 | 注入失败与引用隔离测试 |
| 激活配置 | 原生 enhancement pipeline、真实 Mihomo 校验、原子部署 | 完成 | 部署回滚与固定 Mihomo 测试 |
| 单个/全部/定时更新订阅 | 串行更新、失败汇总、活动订阅部署失败恢复 | 完成 | HTTP fixture 与回滚测试 |
| 订阅下载代理 | 直连、GSettings 系统代理、Mihomo mixed-port | 完成（Linux） | 平台后端可扩展；其他系统由 P9 接入 |
| 不安全证书、超时、User-Agent、更新间隔 | 每订阅强类型设置 | 完成 | 输入边界与 round-trip 测试 |
| 流量、到期、主页和建议更新间隔 | 解析标准订阅响应头并发布非敏感快照 | 完成 | header parser 覆盖 |
| Merge/Rules/Proxies/Groups 增强 | source 独占四个 Rust enhancement 文件 | 完成 | 复制、级联删除、引用校验测试 |
| 原始 YAML 编辑 | 一次性脱敏事件、缓存高亮、事务保存与回滚 | 完成 | 活动与非活动 source 校验测试 |
| 规则/代理/代理组可视化编辑 | 三段 sequence 列表增删重排；任意 YAML 字段保留 | 完成 | 语义 round-trip 与必填字段测试 |
| 固定兼容增强脚本 | 开发者维护的 Rust transforms | 完成 | golden enhancement 与 pipeline 测试 |
| 用户 JavaScript 增强脚本 | 不执行、不导入；未来仅在真实需求下设计版本化 schema | 不采用 | 移除 Boa；CVR 导入报告剥离结果 |
| 记忆各组节点 | 选择成功后写入当前 source，激活时恢复可用节点 | 完成 | app/Mihomo actor 端到端测试 |
| 切换后自动关闭连接 | profile 切换关闭旧连接；节点切换只关闭旧 chain | 完成 | 无真实网络的定向连接测试 |
| 增强、校验与部署诊断 | 展示原生 transforms、固定 pipeline 和最近失败阶段 | 完成 | 阶段分类测试 |
| 打开配置目录/文件 | 平台文件管理器 adapter | 后续阶段（P9） | UI 不直接执行系统命令 |
| 备份、恢复、导入和导出 | 独立有界备份系统 | 后续阶段（P11） | 不复用 CVR 可写目录 |

删除当前 profile 当前采用“清空 catalog.current，但保留已验证并部署的 runtime 继续运行”。
这一行为避免在编辑操作中突然中断网络；若后续提供自动切换或停止核心策略，将作为显式设置，
不会静默改变。

## P8

| CVR 能力 | rsclash 原生实现 | 状态 | 验证或边界 |
| --- | --- | --- | --- |
| ProxyViewV1 | 稳定 core/provider record ID 与三类 unresolved 原因 | 完成 | 确定性和同名 provider 测试 |
| 代理组与当前节点 | 展开、折叠、当前选择和嵌套组成员 | 完成 | Mihomo snapshot 驱动 |
| 节点查找与布局 | 普通/详细、文本/正则/全词、名称/延迟排序 | 完成 | `regex-lite`，成员 `show_rows` |
| 节点/组/全部测速 | 按 core/provider source 调用对应 controller API | 完成 | actor 串行化与 fake API |
| Proxy provider 操作 | 单个/全部更新、provider/节点健康检查 | 完成 | 操作后 metadata 刷新 |
| 节点选择记忆与连接清理 | profile 持久化、tray 同步、只关闭旧 chain | 完成 | P7 actor 端到端测试 |
| 代理链、规则、连接、日志和实时指标 | 后续 P8 独立提交 | 进行中 | 本矩阵随提交更新 |
