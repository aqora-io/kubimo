use clap::Args;
use object_store::{ClientOptions, aws::AmazonS3Builder};

#[derive(Args, Debug, Default, Clone)]
pub struct GlobalArgs {
    #[arg(global = true, long, short = 'k', env = "AWS_ALLOW_HTTP")]
    allow_http: bool,
    #[arg(
        global = true,
        long,
        short = 'i',
        env = "AWS_ALLOW_INVALID_CERTIFICATES"
    )]
    allow_insecure: bool,
}

pub struct Context {
    pub s3: AmazonS3Builder,
}

impl Context {
    pub fn new(args: GlobalArgs) -> Self {
        Self {
            s3: AmazonS3Builder::from_env().with_client_options(
                ClientOptions::new()
                    .with_allow_http(args.allow_http)
                    .with_allow_invalid_certificates(args.allow_insecure),
            ),
        }
    }
}
