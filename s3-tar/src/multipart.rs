use clap::Args;

#[derive(Args, Debug, Clone)]
pub struct MultipartOptions {
    #[clap(long, short = 'c', default_value_t = 10_000_000)]
    pub chunk_size: usize,
    #[clap(long, short = 'n', default_value_t = 10)]
    pub concurrency: usize,
}
