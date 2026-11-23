// The default Windows heap serializes allocations across threads; this workload allocates
// and frees large numbers of chunk-sized buffers on every worker, so use mimalloc instead.
#[cfg(windows)]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> std::process::ExitCode {
    linear_converter::run()
}
