//! 单页前端资源；构建时直接嵌进二进制，部署只需要一个可执行文件。

pub const INDEX: &str = include_str!("../assets/index.html");
pub const APP_JS: &str = include_str!("../assets/app.js");
pub const STYLE_CSS: &str = include_str!("../assets/style.css");
