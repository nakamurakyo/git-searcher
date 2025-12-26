use graphql_client::GraphQLQuery;

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "src/dummy.graphql",
    query_path  = "src/query.graphql",
    response_derives = "Debug"
)]
pub struct FileBlame;
