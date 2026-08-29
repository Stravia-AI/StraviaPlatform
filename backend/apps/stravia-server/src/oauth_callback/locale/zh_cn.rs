use super::CallbackCopy;

pub(super) static COPY: CallbackCopy = CallbackCopy {
    lang: "zh-CN",
    complete_title: "OAuth 已完成",
    complete_message: "授权成功。请返回 Stravia 保存 Provider。",
    failed_title: "OAuth 无法完成",
    failed_message: "授权失败。请返回 Stravia 查看详情和重试指引。",
};
