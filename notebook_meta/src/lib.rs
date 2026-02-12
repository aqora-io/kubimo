use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotebookMeta {
    pub version: MetaVersion,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub funcs: Vec<Function>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum MetaVersion {
    V1,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Function {
    #[serde(default, rename = "async", skip_serializing_if = "std::ops::Not::not")]
    pub is_async: bool,
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<FunctionArg>,
    #[serde(default, rename = "return", skip_serializing_if = "Option::is_none")]
    pub return_ty: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docs: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FunctionArg {
    pub name: String,
    #[serde(
        default,
        rename = "keyword",
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub is_keyword: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ty: Option<String>,
    #[serde(default, rename = "default", skip_serializing_if = "Option::is_none")]
    pub default_value: Option<String>,
}
