mod mutation;
mod query;

use async_graphql::{EmptySubscription, Schema as BaseSchema};

pub type Schema = BaseSchema<query::Query, mutation::Mutation, EmptySubscription>;
