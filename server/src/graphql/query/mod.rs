use async_graphql::*;

pub mod node;
pub mod workspace;

#[derive(Default, MergedObject)]
pub struct Query(node::NodeQuery, workspace::WorkspaceQuery);
