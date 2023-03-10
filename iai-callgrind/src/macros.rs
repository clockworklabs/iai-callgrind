//! Contains macros which together define a benchmark harness that can be used in place of the
//! standard benchmark harness. This allows the user to run Iai benchmarks with `cargo bench`.

/// Macro which expands to a benchmark harness.
///
/// Currently, using Iai-callgrind requires disabling the benchmark harness generated automatically
/// by rustc. This can be done like so:
///
/// ```toml
/// [[bench]]
/// name = "my_bench"
/// harness = false
/// ```
///
/// To be able to run any iai-callgrind benchmarks, you'll also need the `iai-callgrind-runner`
/// installed with the binary somewhere in your $PATH for example with
///
/// ```shell
/// cargo install iai-callgrind-runner
/// ```
///
/// `my_bench` must be a rust file inside the 'benches' directory, like so:
///
/// `benches/my_bench.rs`
///
/// Since we've disabled the default benchmark harness, we need to add our own:
///
/// ```ignore
/// use iai_callgrind::main;
///
/// // `#[inline(never)]` is important! Without it there won't be any metrics
/// #[inline(never)]
/// fn bench_method1() {
/// }
///
/// #[inline(never)]
/// fn bench_method2() {
/// }
///
/// main!(bench_method1, bench_method2);
/// ```
///
/// The `iai_callgrind::main` macro expands to a `main` function which runs all of the benchmarks.
#[macro_export]
macro_rules! main {
    ( $( $func_name:ident ),+ $(,)* ) => {
        mod iai_wrappers {
            $(
                pub fn $func_name() {
                    let _ = $crate::black_box(super::$func_name());
                }
            )+
        }

        #[inline(never)]
        fn run(benchmarks: &[&(&'static str, fn())], this_args: std::env::Args) {
            let exe = option_env!("CARGO_BIN_EXE_iai-callgrind-runner").unwrap_or("iai-callgrind-runner");
            let mut cmd = std::process::Command::new(exe);

            cmd.arg(module_path!());

            for bench in benchmarks {
                cmd.arg(format!("--iai-bench={}", bench.0));
            }

            let mut args = Vec::with_capacity(20);
            args.extend(this_args);
            let status = cmd
                .args(args)
                .status()
                .expect("Failed to run benchmarks. Is iai-callgrind-runner installed and iai-callgrind-runner in your $PATH?");

            if !status.success() {
                panic!(
                    "Failed to run iai-callgrind-runner. Exit code: {}",
                    status
                );
            }
        }

        fn main() {
            let benchmarks : &[&(&'static str, fn())]= $crate::black_box(&[
                $(
                    &(stringify!($func_name), iai_wrappers::$func_name),
                )+
            ]);

            let mut args_iter = $crate::black_box(std::env::args()).skip(1);
            if args_iter.next().as_ref().map_or(false, |value| value == "--iai-run") {
                let index = $crate::black_box(args_iter
                    .next()
                    .and_then(|arg| arg.parse::<usize>().ok())
                    .expect("Error parsing index"));
                benchmarks[index].1();
            } else {
                run($crate::black_box(benchmarks), $crate::black_box(std::env::args()));
            };
        }
    }
}