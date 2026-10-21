//! 下发内容的通用后处理（不认识的 provider 也适用）。
//!
//! 这里放的是**与具体上游无关**的内容改写：判定依据全部来自文本本身，不在
//! 这里出现 provider 分支（provider 只决定「要不要开」，见
//! `upstream::provider_loop::frame_policy_of`）。
//!
//!   think_tags.rs   剥离响应**开头**的 `<think>…</think>` 块（默认关）

pub mod think_tags;
