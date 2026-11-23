---
status: accepted
---

# 由 base Vendor 在运行时注册远端目录中的兼容 Provider Profile

## 迁移基线

插件化前原生行为以 commit `4b7cabcbb3894d7f9e5226849e17c5b663dbf8cb` 为比较基线，而不是从插件迁移后的实现倒推。本节仅保留历史参照；现行目录与账号发现规则由 [Vendor Wasm 插件设计](../design/vendor-plugins.md)拥有。

## 决定

`base` 在运行时注册远端目录中能映射到自身协议和认证实现的 Provider Profile，而不是让构建时清单决定全部身份。这样新增兼容入口不必重新构建插件；目录数据仍不能凭空增加协议、认证能力或混用专属 Vendor。离线回退到最近成功快照，没有快照才用内嵌清单；三种来源都按插件实际支持能力校验。

Core 的 Canonical Model 与 base 供应商目录独立刷新，不要求共享原子 revision。某一来源失败要报告并保留其有效快照，不能把其他来源的成功伪报为共同成功。账号发现或管理员决定的模型集合不能由 Canonical Model 富化扩充；富化只补缺失元数据，也不能冒充上游账号发现成功。目录删项不删除既有连接或历史，但该目录入口不再用于新建。

本决定局部修订 [ADR-0018](0018-use-revisioned-stravia-model-catalog.md) 的跨模块原子 revision 与 scope 失败行为。Profile、模型发现与刷新细节见 [Vendor Wasm 插件设计](../design/vendor-plugins.md)；参考价格富化的限制见[成本与计费设计](../design/cost-and-billing.md)。
