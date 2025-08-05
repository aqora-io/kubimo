mod workspace;

use async_graphql::*;

#[derive(Default, MergedObject)]
pub struct Mutation(workspace::WorkspaceMutation);
