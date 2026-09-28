use crate::method::{ClientRequest, Method};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenVikingList;
impl Method for OpenVikingList {
    const NAME: &'static str = "openviking/list";
    type Params = OpenVikingListParams;
    type Result = OpenVikingListResult;
}
impl ClientRequest for OpenVikingList {}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenVikingListParams {
    pub uri: String,
    pub recursive: bool,
    pub limit: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenVikingListResult {
    pub uri: String,
    pub entries: Vec<OpenVikingEntry>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenVikingEntry {
    pub name: String,
    pub uri: String,
    pub is_dir: bool,
    #[serde(rename = "abstract", default)]
    pub abstract_text: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenVikingRead;
impl Method for OpenVikingRead {
    const NAME: &'static str = "openviking/read";
    type Params = OpenVikingReadParams;
    type Result = OpenVikingReadResult;
}
impl ClientRequest for OpenVikingRead {}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenVikingReadParams {
    pub uri: String,
    pub offset: usize,
    pub limit: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenVikingReadResult {
    pub uri: String,
    pub content: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenVikingWrite;
impl Method for OpenVikingWrite {
    const NAME: &'static str = "openviking/write";
    type Params = OpenVikingWriteParams;
    type Result = OpenVikingWriteResult;
}
impl ClientRequest for OpenVikingWrite {}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenVikingWriteParams {
    pub uri: String,
    pub content: String,
    pub mode: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenVikingWriteResult {
    pub uri: String,
    pub result: serde_json::Value,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenVikingDelete;
impl Method for OpenVikingDelete {
    const NAME: &'static str = "openviking/delete";
    type Params = OpenVikingDeleteParams;
    type Result = OpenVikingDeleteResult;
}
impl ClientRequest for OpenVikingDelete {}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenVikingDeleteParams {
    pub uri: String,
    pub recursive: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenVikingDeleteResult {
    pub uri: String,
    pub result: serde_json::Value,
}
