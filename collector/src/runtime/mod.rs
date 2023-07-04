use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};

use thousands::Separable;

use benchlib::comm::messages::{BenchmarkMessage, BenchmarkResult, BenchmarkStats};
pub use benchmark::{
    prepare_runtime_benchmark_suite, runtime_benchmark_dir, BenchmarkFilter, BenchmarkGroup,
    BenchmarkSuite, CargoIsolationMode,
};
use database::{ArtifactIdNumber, CollectionId, Connection};

use crate::utils::git::get_rustc_perf_commit;
use crate::CollectorCtx;

mod benchmark;

pub const DEFAULT_RUNTIME_ITERATIONS: u32 = 5;

/// Perform a series of runtime benchmarks using the provided `rustc` compiler.
/// The runtime benchmarks are looked up in `benchmark_dir`, which is expected to be a path
/// to a Cargo crate. All binaries built by that crate are expected to be runtime benchmark
/// groups that use `benchlib`.
pub async fn bench_runtime(
    mut conn: Box<dyn Connection>,
    suite: BenchmarkSuite,
    collector: &CollectorCtx,
    filter: BenchmarkFilter,
    iterations: u32,
) -> anyhow::Result<()> {
    let total_benchmark_count = suite.total_benchmark_count();
    let filtered = suite.filtered_benchmark_count(&filter);
    println!(
        "Executing {} benchmarks ({} filtered out)\n",
        filtered,
        total_benchmark_count - filtered
    );

    let rustc_perf_version = get_rustc_perf_commit();

    let mut benchmark_index = 0;
    for group in suite.groups {
        if !collector.start_runtime_step(conn.as_mut(), &group).await {
            eprintln!("skipping {} -- already benchmarked", group.name);
            continue;
        }

        for message in execute_runtime_benchmark_binary(&group.binary, &filter, iterations)? {
            let message = message.map_err(|err| {
                anyhow::anyhow!(
                    "Cannot parse BenchmarkMessage from benchmark {}: {err:?}",
                    group.binary.display()
                )
            })?;
            match message {
                BenchmarkMessage::Result(result) => {
                    benchmark_index += 1;
                    println!(
                        "Finished {}/{} ({}/{})",
                        group.name, result.name, benchmark_index, filtered
                    );

                    print_stats(&result);
                    record_stats(
                        conn.as_ref(),
                        collector.artifact_row_id,
                        &rustc_perf_version,
                        result,
                    )
                    .await;
                }
            }
        }

        collector.end_runtime_step(conn.as_mut(), &group).await;
    }

    Ok(())
}

/// Records the results (stats) of a benchmark into the database.
async fn record_stats(
    conn: &dyn Connection,
    artifact_id: ArtifactIdNumber,
    rustc_perf_version: &str,
    result: BenchmarkResult,
) {
    async fn record<'a>(
        conn: &'a dyn Connection,
        artifact_id: ArtifactIdNumber,
        collection_id: CollectionId,
        result: &'a BenchmarkResult,
        value: Option<u64>,
        metric: &'a str,
    ) {
        if let Some(value) = value {
            conn.record_runtime_statistic(
                collection_id,
                artifact_id,
                &result.name,
                metric,
                value as f64,
            )
            .await;
        }
    }

    for stat in &result.stats {
        let collection_id = conn.collection_id(rustc_perf_version).await;

        record(
            conn,
            artifact_id,
            collection_id,
            &result,
            stat.instructions,
            "instructions:u",
        )
        .await;
        record(
            conn,
            artifact_id,
            collection_id,
            &result,
            stat.cycles,
            "cycles:u",
        )
        .await;
        record(
            conn,
            artifact_id,
            collection_id,
            &result,
            stat.branch_misses,
            "branch-misses",
        )
        .await;
        record(
            conn,
            artifact_id,
            collection_id,
            &result,
            stat.cache_misses,
            "cache-misses",
        )
        .await;
        record(
            conn,
            artifact_id,
            collection_id,
            &result,
            Some(stat.wall_time.as_nanos() as u64),
            "wall-time",
        )
        .await;
    }
}

/// Starts executing a single runtime benchmark group defined in a binary crate located in
/// `runtime-benchmarks`. The binary is expected to use benchlib's `BenchmarkGroup` to execute
/// a set of runtime benchmarks and print `BenchmarkMessage`s encoded as JSON, one per line.
fn execute_runtime_benchmark_binary(
    binary: &Path,
    filter: &BenchmarkFilter,
    iterations: u32,
) -> anyhow::Result<impl Iterator<Item = anyhow::Result<BenchmarkMessage>>> {
    // Turn off ASLR
    let mut command = Command::new("setarch");
    command.arg(std::env::consts::ARCH);
    command.arg("-R");
    command.arg(binary);
    command.arg("run");
    command.arg("--iterations");
    command.arg(&iterations.to_string());

    if let Some(ref exclude) = filter.exclude {
        command.args(["--exclude", exclude]);
    }
    if let Some(ref include) = filter.include {
        command.args(["--include", include]);
    }

    command.stdout(Stdio::piped());
    let mut child = command.spawn()?;
    let stdout = child.stdout.take().unwrap();

    let reader = BufReader::new(stdout);
    let iterator = reader.lines().map(|line| {
        Ok(line.and_then(|line| Ok(serde_json::from_str::<BenchmarkMessage>(&line)?))?)
    });
    Ok(iterator)
}

fn calculate_mean<I: Iterator<Item = f64> + Clone>(iter: I) -> f64 {
    let sum: f64 = iter.clone().sum();
    let count = iter.count();
    sum / count as f64
}

fn print_stats(result: &BenchmarkResult) {
    fn print_metric<F: Fn(&BenchmarkStats) -> Option<u64>>(
        result: &BenchmarkResult,
        name: &str,
        f: F,
    ) {
        let name = format!("[{name}]");
        let has_data = result.stats.iter().map(&f).all(|v| v.is_some());
        if has_data {
            let f = |stats: &BenchmarkStats| -> u64 { f(stats).unwrap() };
            let mean = calculate_mean(result.stats.iter().map(&f).map(|v| v as f64));
            let stddev = calculate_mean(
                result
                    .stats
                    .iter()
                    .map(&f)
                    .map(|v| (v as f64 - mean).powf(2.0)),
            )
            .sqrt();

            println!(
                "{name:>18}: {:>16} (+/- {:>11})",
                (mean as u64).separate_with_commas(),
                (stddev as u64).separate_with_commas()
            );
        } else {
            println!("{name:>20}: Not available");
        }
    }

    print_metric(result, "Instructions", |m| m.instructions);
    print_metric(result, "Cycles", |m| m.cycles);
    print_metric(result, "Wall time [µs]", |m| {
        Some(m.wall_time.as_micros() as u64)
    });
    print_metric(result, "Branch misses", |m| m.branch_misses);
    print_metric(result, "Cache misses", |m| m.cache_misses);
    println!();
}
