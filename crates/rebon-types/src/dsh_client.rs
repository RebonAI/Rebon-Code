use serde::{Deserialize, Serialize};

/// 插件包提供的 DSH 客户端及其本地后端路由。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DshClientManifest {
    /// 客户端入口，相对于插件包根目录。
    pub entry: String,
    /// 本地后端状态文件，相对于插件包根目录。
    pub runtime: String,
    /// 附带 capability 的后端路由前缀。
    pub route: String,
    pub media: Option<DshClientMedia>,
}

impl DshClientManifest {
    pub fn has_safe_route(&self) -> bool {
        let Some(path) = self.route.strip_prefix('/').and_then(|route| route.strip_suffix('/')) else {
            return false;
        };
        !path.is_empty()
            && path.split('/').all(|segment| {
                !segment.is_empty()
                    && segment != "."
                    && segment != ".."
                    && segment.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~')
                    })
            })
    }
}

/// 客户端声明的媒体能力；实际程序由应用用户选择。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DshClientMedia {
    pub ffmpeg: String,
    pub ffprobe: String,
}

#[cfg(test)]
mod tests {
    use super::DshClientManifest;

    #[test]
    fn routes_are_absolute_non_root_path_prefixes_with_segment_boundaries() {
        for (route, expected) in [
            ("/dsh-pet/", true),
            ("/plugins/pet_2/", true),
            ("/", false),
            ("relative/", false),
            ("/pet", false),
            ("/a//b/", false),
            ("/a/../b/", false),
            ("/a/%2e%2e/", false),
            ("/a/?token=x", false),
            ("/a/#fragment", false),
        ] {
            let client = DshClientManifest {
                entry: "client.js".into(),
                runtime: "runtime.json".into(),
                route: route.into(),
                media: None,
            };
            assert_eq!(client.has_safe_route(), expected, "{route}");
        }
    }
}
