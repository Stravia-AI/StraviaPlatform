---
status: accepted
---

# 将显式媒体输入收存为 Artifact

Stravia 抓取并收存结构化媒体或显式上传／导入中的源文件，不因普通消息中提到 URL 就下载。收存用同一 Principal 下的 Artifact 身份，避免模型文件依赖短命外部地址；必要收存失败就终止本次请求，不把源地址直通、删掉附件或继续执行模型。公网抓取必须保证解析、连接和每次重定向都停留在允许的公网 HTTP(S) 边界，平台 Artifact 经归属鉴权直接读取。

收存后的内容可通过内联或签名下载交付，选择发生在发送前，不在上游抓取失败后自动重跑。业务历史和普通 Observation 只保存适用的引用与元数据，不重复保存结构化媒体正文；显式启用的 Debug Wire 按实际传输边界保留媒体，见[请求记录设计](../design/interaction-observation.md)。

单文件 100 MiB；每个 Principal 的暂存限额为 16 个任务、合计 400 MiB，一次 [ADR-0092](0092-replace-multipart-artifact-upload-with-single-request.md) 定义的单请求上传占一个任务。额度不限制长期已保存总量，超限或底层存储失败不报告成功。归属鉴权后的实际使用才刷新文件保留期；已签发链接的访问不续期。文件到期拒绝新使用，物理删除仍保护进行中读取和未到期的下载授权。

收存、交付与保留契约见 [Agent Core](../design/agent-core.md)。读取所得引用使用 [ADR-0091 的路径语法](0091-read-paths-with-colon-selectors-and-plain-fields.md)。
