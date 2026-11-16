//! HTML 的正文选择与 Markdown alternate 发现，共用一次 DOM 转换。

use std::sync::Arc;

use htmd::element_handler::Handlers;
use htmd::{Element, HtmlToMarkdown};
use parking_lot::Mutex;

#[derive(Default)]
pub(super) struct HtmlPage {
    pub text: String,
    pub title: Option<String>,
    pub alternates: Vec<String>,
}

#[derive(Debug, Default)]
struct Selection {
    title: Option<String>,
    alternates: Vec<String>,
    main: Vec<String>,
    articles: Vec<String>,
    region_depth: usize,
}

pub(super) fn render(html: &str) -> std::io::Result<HtmlPage> {
    let selected = Arc::new(Mutex::new(Selection::default()));
    let title = Arc::clone(&selected);
    let links = Arc::clone(&selected);
    let regions = Arc::clone(&selected);
    let converter = HtmlToMarkdown::builder()
        .skip_tags(vec![
            "script", "style", "noscript", "svg", "template", "nav", "footer", "form", "button",
        ])
        .add_handler(
            vec!["title"],
            move |handlers: &dyn Handlers, element: Element| {
                let text = handlers.walk_children(element.node).content;
                let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
                if !text.is_empty() {
                    title.lock().title = Some(text.chars().take(200).collect());
                }
                None
            },
        )
        .add_handler(vec!["link"], move |_: &dyn Handlers, element: Element| {
            let attr = |key: &str| {
                element
                    .attrs
                    .iter()
                    .find(|attr| attr.name.local.as_ref() == key)
                    .map_or("", |attr| attr.value.as_ref())
            };
            if attr("rel")
                .split_whitespace()
                .any(|rel| rel.eq_ignore_ascii_case("alternate"))
                && matches!(attr("type"), "text/markdown" | "text/x-markdown")
                && !attr("href").is_empty()
            {
                links.lock().alternates.push(attr("href").to_owned());
            }
            None
        })
        .add_handler(
            vec!["main", "article", "div", "section"],
            move |handlers: &dyn Handlers, element: Element| {
                let main = element.tag == "main"
                    || element.attrs.iter().any(|attr| {
                        attr.name.local.as_ref() == "role" && attr.value.as_ref() == "main"
                    });
                if (!main && element.tag != "article") || regions.lock().region_depth != 0 {
                    return handlers.fallback(element);
                }
                regions.lock().region_depth += 1;
                let content = handlers.walk_children(element.node).content;
                let mut selected = regions.lock();
                selected.region_depth -= 1;
                if main {
                    selected.main.push(content);
                } else {
                    selected.articles.push(content);
                }
                None
            },
        )
        .build();
    let remainder = converter.convert(html)?;
    drop(converter);
    let mut selected = Arc::try_unwrap(selected)
        .expect("转换器已释放所有回调")
        .into_inner();
    let body = if selected.main.iter().any(|text| !text.trim().is_empty()) {
        selected.main.join("\n\n")
    } else if selected.articles.iter().any(|text| !text.trim().is_empty()) {
        selected.articles.join("\n\n")
    } else {
        remainder
    };
    let text = match &selected.title {
        Some(title) => format!("# {title}\n\n{}", body.trim()),
        None => body.trim().to_owned(),
    };
    Ok(HtmlPage {
        text,
        title: selected.title.take(),
        alternates: selected.alternates,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_main_without_navigation_and_preserves_nested_details() {
        let page = render("<title>API &amp; docs</title><nav>menu</nav><div>outside</div><main><article><h2>Parameters</h2><details><summary>temperature</summary><p>0 to 2</p></details><pre><code>request()</code></pre></article></main><footer>legal</footer>").unwrap();
        assert_eq!(page.title.as_deref(), Some("API & docs"));
        assert!(page.text.contains("temperature"));
        assert!(page.text.contains("0 to 2"));
        assert!(page.text.contains("request()"));
        for noise in ["menu", "outside", "legal"] {
            assert!(!page.text.contains(noise));
        }
        let article = render(
            "<nav>menu</nav><article><p>first</p></article><article><p>second</p></article>",
        )
        .unwrap();
        assert!(article.text.contains("first"));
        assert!(article.text.contains("second"));
    }
}
