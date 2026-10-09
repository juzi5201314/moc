//! MOC internal URI 的严格手工解析器。

/// URI 语法错误；资源 scheme 是否注册由 `ResourceRouter` 判断。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UriError {
    pub code: &'static str,
    pub message: String,
}

impl UriError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: "E_URI_INVALID",
            message: message.into(),
        }
    }
}

/// 已解析的内部 URI。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InternalUri {
    raw: String,
    scheme: String,
    authority: String,
    segments: Vec<String>,
}

impl InternalUri {
    pub fn parse(input: &str) -> Result<Self, UriError> {
        if input.is_empty() || input.chars().any(char::is_control) {
            return Err(UriError::invalid(
                "URI is empty or contains control characters",
            ));
        }
        if input.contains(['\\', '?', '#']) {
            return Err(UriError::invalid(
                "URI cannot contain backslashes, queries, or fragments",
            ));
        }
        let (scheme, remainder) = input
            .split_once("://")
            .ok_or_else(|| UriError::invalid("URI must use scheme://authority/path form"))?;
        if scheme.is_empty()
            || !scheme.bytes().enumerate().all(|(index, byte)| {
                if index == 0 {
                    byte.is_ascii_lowercase()
                } else {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'+' | b'.' | b'-')
                }
            })
        {
            return Err(UriError::invalid(
                "scheme must be a lowercase ASCII identifier",
            ));
        }
        let (authority, path) = remainder.split_once('/').unwrap_or((remainder, ""));
        if authority.is_empty() {
            return Err(UriError::invalid("authority must not be empty"));
        }
        let authority = decode_component(authority)?;
        if authority.is_empty() || authority == "." || authority == ".." {
            return Err(UriError::invalid("invalid authority"));
        }
        let mut segments = Vec::new();
        if !path.is_empty() {
            for raw_segment in path.split('/') {
                if raw_segment.is_empty() {
                    return Err(UriError::invalid("URI paths cannot contain empty segments"));
                }
                let segment = decode_component(raw_segment)?;
                if segment.is_empty() || segment == "." || segment == ".." {
                    return Err(UriError::invalid("URI path contains an invalid segment"));
                }
                segments.push(segment);
            }
        }
        Ok(Self {
            raw: input.to_owned(),
            scheme: scheme.to_owned(),
            authority,
            segments,
        })
    }

    pub fn raw(&self) -> &str {
        &self.raw
    }

    pub fn scheme(&self) -> &str {
        &self.scheme
    }

    pub fn authority(&self) -> &str {
        &self.authority
    }

    pub fn path_segments(&self) -> &[String] {
        &self.segments
    }

    pub fn two_segments(&self, code: &'static str) -> Result<[&str; 2], super::ResourceError> {
        let [first, second] = self.segments.as_slice() else {
            return Err(super::ResourceError::new(
                code,
                "URI path must contain exactly two segments",
            ));
        };
        Ok([first, second])
    }
}

fn decode_component(input: &str) -> Result<String, UriError> {
    let decoded = if !input.as_bytes().contains(&b'%') {
        input.to_owned()
    } else {
        let mut bytes = Vec::with_capacity(input.len());
        let mut chars = input.as_bytes().iter().copied();
        while let Some(byte) = chars.next() {
            if byte != b'%' {
                bytes.push(byte);
                continue;
            }
            let Some(high) = chars.next() else {
                return Err(UriError::invalid("percent escape is incomplete"));
            };
            let Some(low) = chars.next() else {
                return Err(UriError::invalid("percent escape is incomplete"));
            };
            let Some(high) = hex(high) else {
                return Err(UriError::invalid("invalid percent escape"));
            };
            let Some(low) = hex(low) else {
                return Err(UriError::invalid("invalid percent escape"));
            };
            bytes.push((high << 4) | low);
        }
        String::from_utf8(bytes).map_err(|_| UriError::invalid("percent escape is not UTF-8"))?
    };
    if decoded.chars().any(char::is_control) || decoded.contains(['\\', '/', '?', '#']) {
        return Err(UriError::invalid(
            "URI contains invalid characters after decoding",
        ));
    }
    Ok(decoded)
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// 已实现的 MOC-native scheme。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceScheme {
    Artifact,
    Skill,
    Local,
    History,
    OpenViking,
}

impl ResourceScheme {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Artifact => "artifact",
            Self::Skill => "skill",
            Self::Local => "local",
            Self::History => "history",
            Self::OpenViking => "viking",
        }
    }

    pub fn parse(input: &str) -> Option<Self> {
        match input {
            "artifact" => Some(Self::Artifact),
            "skill" => Some(Self::Skill),
            "local" => Some(Self::Local),
            "history" => Some(Self::History),
            "viking" => Some(Self::OpenViking),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_uri_round_trips_and_decodes_path() {
        let uri = InternalUri::parse("skill://demo/references/a%20b.md").expect("URI 合法");
        assert_eq!(uri.raw(), "skill://demo/references/a%20b.md");
        assert_eq!(uri.scheme(), "skill");
        assert_eq!(uri.authority(), "demo");
        assert_eq!(
            uri.path_segments(),
            &["references".to_owned(), "a b.md".to_owned()]
        );
    }

    #[test]
    fn invalid_uri_rejects_traversal_query_fragment_and_unpaired_escape() {
        for input in [
            "skill://demo/../SKILL.md",
            "skill://demo/a//b",
            "skill://demo/a?x=1",
            "skill://demo/a#part",
            "skill://demo/%F",
            "skill://demo/a%2Fb",
            "skill://demo/%0A",
            "Skill://demo",
        ] {
            assert_eq!(
                InternalUri::parse(input).unwrap_err().code,
                "E_URI_INVALID",
                "{input}"
            );
        }
    }
}
