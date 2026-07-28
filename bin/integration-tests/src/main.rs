use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use clap::Parser;
use miden_client::grpc_support::{DEVNET_PROVER_ENDPOINT, TESTNET_PROVER_ENDPOINT};
use miden_client::rpc::Endpoint;
use regex::Regex;
use serde::{Deserialize, Serialize};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

use crate::tests::config::{ClientConfig, NoteTransportEndpoint};

mod generated_tests;
mod tests;

// MAIN
// ================================================================================================

/// Entry point for the integration test binary.
///
/// Parses command line arguments, filters tests based on provided criteria, and runs the selected
/// tests in parallel. Exits with code 1 if any tests fail.
fn main() {
    let args = Args::parse();

    // Initialize tracing (before subprocess check so subprocesses get tracing too)
    init_tracing(args.verbose);

    // If running as a subprocess for a single test, execute it and exit
    if let Some(ref test_name) = args.internal_run_test {
        run_single_test_subprocess(&args, test_name);
        return;
    }

    let all_tests = generated_tests::get_all_tests();
    let filtered_tests = filter_tests(all_tests, &args);

    if args.list {
        list_tests(&filtered_tests);
        return;
    }

    if filtered_tests.is_empty() {
        println!("No tests match the specified filters.");
        return;
    }

    let base_config = match BaseConfig::try_from(args.clone()) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("Error: Failed to create configuration: {e}");
            std::process::exit(1);
        },
    };
    let start_time = Instant::now();

    let results = run_tests_with_retries(filtered_tests, base_config, args.jobs, args.retry_count);

    let total_duration = start_time.elapsed();
    print_summary(&results, total_duration);

    // Exit with error code if any tests failed after retries
    let final_failed_count = results.iter().filter(|r| r.failed()).count();

    if final_failed_count > 0 {
        std::process::exit(1);
    }
}

/// Initializes tracing.
///
/// If `RUST_LOG` is set, it always takes precedence (backwards compatible).
/// Otherwise, when `--verbose` is set, enables info-level tracing for integration tests and
/// the client's test utilities.
///
/// Tracing output is routed to stderr to avoid corrupting subprocess JSON on stdout.
fn init_tracing(verbose: bool) {
    // RUST_LOG always takes precedence for backwards compatibility
    if std::env::var("RUST_LOG").is_ok() {
        tracing_subscriber::registry()
            .with(EnvFilter::from_default_env())
            .with(tracing_subscriber::fmt::layer().with_target(true).with_writer(std::io::stderr))
            .init();
        return;
    }

    if !verbose {
        return;
    }

    tracing_subscriber::registry()
        .with(EnvFilter::new(
            "miden_client_integration_tests=info,miden_client::test_utils=info",
        ))
        .with(tracing_subscriber::fmt::layer().with_target(true).with_writer(std::io::stderr))
        .init();
}

// ARGS
// ================================================================================================

/// Command line arguments for the integration test binary.
#[derive(Parser, Clone)]
#[command(
    name = "miden-client-integration-tests",
    about = "Integration tests for the Miden client library",
    version
)]
struct Args {
    /// Network preset: sets defaults for all components (RPC, prover, note transport).
    /// Options: `devnet`, `testnet`, `localhost`, or a custom RPC endpoint.
    #[arg(short, long, default_value = "localhost", env = "TEST_MIDEN_NETWORK")]
    network: Network,

    /// Override the RPC endpoint from the network preset.
    #[arg(long, env = "TEST_MIDEN_RPC_URL")]
    rpc_url: Option<String>,

    /// Timeout for the RPC requests in milliseconds.
    #[arg(short, long, default_value = "10000")]
    timeout: u64,

    /// Number of tests to run in parallel. Set to 1 for sequential execution.
    #[arg(short, long, default_value_t = num_cpus::get())]
    jobs: usize,

    /// Filter tests by name (supports regex patterns).
    #[arg(short, long)]
    filter: Option<String>,

    /// List all available tests without running them.
    #[arg(long)]
    list: bool,

    /// Only run tests whose names contain this substring.
    #[arg(long)]
    contains: Option<String>,

    /// Exclude tests whose names match this pattern (supports regex).
    #[arg(long)]
    exclude: Option<String>,

    /// Number of times to retry failed tests. Set to 0 to disable retries.
    #[arg(long, default_value = "3")]
    retry_count: usize,

    /// Remote prover endpoint. Accepts "devnet", "testnet", "localhost", or a custom URL.
    /// If unset, defaults based on --network (testnet/devnet use remote provers).
    #[arg(long, env = "TEST_MIDEN_PROVER_URL")]
    prover_url: Option<String>,

    /// Note transport endpoint. Accepts "devnet", "testnet", or a custom URL.
    /// If unset, defaults based on --network.
    #[arg(long, env = "TEST_MIDEN_NOTE_TRANSPORT_URL")]
    note_transport_url: Option<String>,

    /// Enable verbose tracing output (info-level logs from tests and client).
    #[arg(short, long)]
    verbose: bool,

    /// Internal: run a single test by name and exit (hidden from help).
    /// Used by the test runner to spawn subprocesses for parallel execution.
    #[arg(long, hide = true)]
    internal_run_test: Option<String>,
}

/// Base configuration derived from command line arguments.
#[derive(Clone)]
struct BaseConfig {
    rpc_endpoint: Endpoint,
    timeout: u64,
    prover_endpoint: Option<String>,
    note_transport_endpoint: Option<NoteTransportEndpoint>,
    verbose: bool,
}

impl TryFrom<Args> for BaseConfig {
    type Error = anyhow::Error;

    /// Creates a BaseConfig from command line arguments.
    ///
    /// The `--network` flag sets defaults for all components. Individual flags
    /// (`--prover-url`, `--note-transport-url`) override specific components.
    fn try_from(args: Args) -> Result<Self, Self::Error> {
        // --rpc-url overrides the network preset for RPC.
        let endpoint = if let Some(ref rpc_url) = args.rpc_url {
            Endpoint::try_from(rpc_url.as_str())
                .map_err(|e| anyhow::anyhow!("Invalid RPC URL: {rpc_url}: {e}"))?
        } else {
            Endpoint::try_from(args.network.to_rpc_endpoint().as_str())
                .map_err(|e| anyhow::anyhow!("Invalid network: {:?}: {}", args.network, e))?
        };

        let timeout_ms = args.timeout;

        // Resolve prover: explicit flag overrides network preset.
        let prover_endpoint = if let Some(url) = args.prover_url {
            match url.to_lowercase().as_str() {
                "localhost" => None,
                "devnet" => Some(DEVNET_PROVER_ENDPOINT.to_string()),
                "testnet" => Some(TESTNET_PROVER_ENDPOINT.to_string()),
                _ => Some(url),
            }
        } else {
            // Network preset defaults
            match &args.network {
                Network::Testnet => Some(TESTNET_PROVER_ENDPOINT.to_string()),
                Network::Devnet => Some(DEVNET_PROVER_ENDPOINT.to_string()),
                _ => None,
            }
        };

        // Resolve note transport: explicit flag overrides network preset.
        let note_transport_endpoint = if let Some(url) = args.note_transport_url {
            Some(url.parse::<NoteTransportEndpoint>().unwrap())
        } else {
            // Network preset defaults
            match &args.network {
                Network::Testnet => Some(NoteTransportEndpoint::Testnet),
                Network::Devnet => Some(NoteTransportEndpoint::Devnet),
                _ => None,
            }
        };

        Ok(BaseConfig {
            rpc_endpoint: endpoint,
            timeout: timeout_ms,
            prover_endpoint,
            note_transport_endpoint,
            verbose: args.verbose,
        })
    }
}

// TYPE ALIASES
// ================================================================================================

/// Type alias for a test function that takes a ClientConfig and returns a boxed future
type TestFunction = Box<
    dyn Fn(ClientConfig) -> Pin<Box<dyn Future<Output = Result<(), anyhow::Error>>>> + Send + Sync,
>;

// TEST CASE
// ================================================================================================

/// Represents a single test case with its name, category, and associated function.
struct TestCase {
    name: String,
    category: TestCategory,
    function: TestFunction,
}

impl TestCase {
    /// Creates a new TestCase with the given name, category, and function.
    fn new<F, Fut>(name: &str, category: TestCategory, func: F) -> Self
    where
        F: Fn(ClientConfig) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<(), anyhow::Error>> + 'static,
    {
        Self {
            name: name.to_string(),
            category,
            function: Box::new(move |config| Box::pin(func(config))),
        }
    }
}

impl std::fmt::Debug for TestCase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestCase")
            .field("name", &self.name)
            .field("category", &self.category)
            .field("function", &"<function>")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum TestCategory {
    Agglayer,
    Batch,
    Client,
    CustomTransaction,
    Fpi,
    NetworkFpi,
    NetworkTransaction,
    NoteTags,
    Onchain,
    PassThrough,
    PswapTransaction,
    SwapTransaction,
    Transport,
}

impl AsRef<str> for TestCategory {
    fn as_ref(&self) -> &str {
        match self {
            TestCategory::Agglayer => "agglayer",
            TestCategory::Batch => "batch",
            TestCategory::Client => "client",
            TestCategory::CustomTransaction => "custom_transaction",
            TestCategory::Fpi => "fpi",
            TestCategory::NetworkFpi => "network_fpi",
            TestCategory::NetworkTransaction => "network_transaction",
            TestCategory::NoteTags => "note_tags",
            TestCategory::Onchain => "onchain",
            TestCategory::PassThrough => "pass_through",
            TestCategory::PswapTransaction => "pswap_transaction",
            TestCategory::SwapTransaction => "swap_transaction",
            TestCategory::Transport => "transport",
        }
    }
}

/// Represents the result of a single test attempt (one execution of a test case).
#[derive(Debug, Clone)]
struct AttemptResult {
    name: String,
    category: String,
    passed: bool,
    duration: Duration,
    error_message: Option<String>,
    /// Captured stdout from the test (only shown for failed tests).
    captured_output: Option<String>,
}

/// Represents the final result of a test case, including all retry attempts.
#[derive(Debug, Clone)]
struct TestResult {
    name: String,
    category: String,
    attempts: Vec<AttemptResult>,
}

impl TestResult {
    /// Returns `true` if the test ultimately passed (last attempt passed).
    fn passed(&self) -> bool {
        self.attempts.last().is_some_and(|a| a.passed)
    }

    /// Returns `true` if the test ultimately failed (last attempt failed).
    fn failed(&self) -> bool {
        !self.passed()
    }

    /// Returns the number of retries (attempts beyond the first).
    fn retries(&self) -> usize {
        self.attempts.len().saturating_sub(1)
    }

    /// Returns `true` if the test was flaky (failed initially but passed on retry).
    fn is_flaky(&self) -> bool {
        self.passed() && self.retries() > 0
    }

    /// Returns the duration of the last attempt.
    fn duration(&self) -> Duration {
        self.attempts.last().map_or(Duration::ZERO, |a| a.duration)
    }

    /// Returns the error message from the last attempt, if any.
    fn error_message(&self) -> Option<&str> {
        self.attempts.last().and_then(|a| a.error_message.as_deref())
    }

    /// Returns the captured output from the last attempt, if any.
    fn captured_output(&self) -> Option<&str> {
        self.attempts.last().and_then(|a| a.captured_output.as_deref())
    }
}

// SUBPROCESS RESULT
// ================================================================================================

/// Result type serialized by subprocess and parsed by parent process.
/// Uses f64 for duration (seconds) to avoid custom serde implementations.
#[derive(Debug, Serialize, Deserialize)]
struct SubprocessResult {
    name: String,
    category: String,
    passed: bool,
    duration_secs: f64,
    error_message: Option<String>,
}

impl SubprocessResult {
    fn passed(name: &str, category: &TestCategory, duration: Duration) -> Self {
        Self {
            name: name.to_string(),
            category: category.as_ref().to_string(),
            passed: true,
            duration_secs: duration.as_secs_f64(),
            error_message: None,
        }
    }

    fn failed(name: &str, category: &TestCategory, duration: Duration, error: &str) -> Self {
        Self {
            name: name.to_string(),
            category: category.as_ref().to_string(),
            passed: false,
            duration_secs: duration.as_secs_f64(),
            error_message: Some(error.to_string()),
        }
    }

    fn error(name: &str, error: &str) -> Self {
        Self {
            name: name.to_string(),
            category: "unknown".to_string(),
            passed: false,
            duration_secs: 0.0,
            error_message: Some(error.to_string()),
        }
    }
}

/// Runs a single test in subprocess mode.
///
/// This function is called when the binary is invoked with `--internal-run-test`.
/// It executes the named test, captures the result, and outputs it as JSON to stdout.
/// All other stdout from the test is preserved and will be captured by the parent process.
fn run_single_test_subprocess(args: &Args, test_name: &str) {
    let all_tests = generated_tests::get_all_tests();
    let test = all_tests.into_iter().find(|t| t.name == test_name);

    let Some(test) = test else {
        let result = SubprocessResult::error(test_name, "Test not found");
        println!("{}", serde_json::to_string(&result).unwrap());
        std::process::exit(1);
    };

    let base_config = match BaseConfig::try_from(args.clone()) {
        Ok(c) => c,
        Err(e) => {
            let result = SubprocessResult::error(test_name, &e.to_string());
            println!("{}", serde_json::to_string(&result).unwrap());
            std::process::exit(1);
        },
    };

    let start = Instant::now();
    let rt = tokio::runtime::Runtime::new().unwrap();

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rt.block_on(async {
            let config = ClientConfig::new(base_config.rpc_endpoint.clone(), base_config.timeout)
                .with_prover_endpoint(base_config.prover_endpoint.clone())
                .with_note_transport_endpoint(base_config.note_transport_endpoint.clone());
            (test.function)(config).await
        })
    }));

    let duration = start.elapsed();
    let subprocess_result = match result {
        Ok(Ok(_)) => SubprocessResult::passed(test_name, &test.category, duration),
        Ok(Err(e)) => {
            SubprocessResult::failed(test_name, &test.category, duration, &format_error_report(e))
        },
        Err(panic) => {
            let msg = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "Unknown panic".into());
            SubprocessResult::failed(test_name, &test.category, duration, &msg)
        },
    };

    // Output result as JSON to stdout (will be captured by parent)
    println!("{}", serde_json::to_string(&subprocess_result).unwrap());
    std::process::exit(if subprocess_result.passed { 0 } else { 1 });
}

/// Filters the list of tests based on command line arguments.
///
/// Applies regex patterns, substring matching, and exclusion filters to select which tests should
/// be executed.
fn filter_tests(tests: Vec<TestCase>, args: &Args) -> Vec<TestCase> {
    let mut filtered_tests = tests;

    // Apply filter (regex pattern on test names)
    if let Some(ref filter_pattern) = args.filter {
        if let Ok(regex) = Regex::new(filter_pattern) {
            filtered_tests.retain(|test| regex.is_match(&test.name));
        } else {
            eprintln!("Warning: Invalid regex pattern in filter: {filter_pattern}");
        }
    }

    // Apply contains filter
    if let Some(ref contains) = args.contains {
        filtered_tests.retain(|test| test.name.contains(contains));
    }

    // Apply exclude filter
    if let Some(ref exclude_pattern) = args.exclude {
        if let Ok(regex) = Regex::new(exclude_pattern) {
            filtered_tests.retain(|test| !regex.is_match(&test.name));
        } else {
            eprintln!("Warning: Invalid regex pattern in exclude: {exclude_pattern}");
        }
    }

    filtered_tests
}

/// Prints all available tests organized by category.
///
/// Used when the --list flag is provided to show what tests are available without actually running
/// them.
fn list_tests(tests: &[TestCase]) {
    println!("Available tests:");
    println!("================");

    let mut tests_by_category: BTreeMap<TestCategory, Vec<&TestCase>> = BTreeMap::new();
    for test in tests {
        tests_by_category.entry(test.category.clone()).or_default().push(test);
    }

    for (category, tests) in tests_by_category {
        println!("\n{}:", category.as_ref().to_uppercase());
        for test in tests {
            println!("  - {}", test.name);
        }
    }

    println!("\nTotal: {} tests", tests.len());
}

/// Formats an error with its full chain
fn format_error_report(error: anyhow::Error) -> String {
    let mut output = String::new();
    let mut first = true;

    for err in error.chain() {
        if !first {
            output.push_str("\n  Caused by: ");
        }
        output.push_str(&format!("{err}"));
        first = false;
    }

    output
}

/// Runs tests with retries for failed tests.
///
/// Executes tests in parallel, then retries any failures up to `retry_count` times.
/// Returns the final results for all tests, including attempt history.
fn run_tests_with_retries(
    tests: Vec<TestCase>,
    base_config: BaseConfig,
    jobs: usize,
    retry_count: usize,
) -> Vec<TestResult> {
    let initial_attempts = run_tests_parallel(tests, base_config.clone(), jobs, false);

    let mut results: Vec<TestResult> = initial_attempts
        .into_iter()
        .map(|attempt| TestResult {
            name: attempt.name.clone(),
            category: attempt.category.clone(),
            attempts: vec![attempt],
        })
        .collect();

    for retry in 1..=retry_count {
        let failed_names: Vec<&str> =
            results.iter().filter(|r| r.failed()).map(|r| r.name.as_str()).collect();

        if failed_names.is_empty() {
            break;
        }

        println!("\n=== RETRY ATTEMPT {retry}/{retry_count} ===");
        println!("Retrying {} failed test(s)...", failed_names.len());

        let tests_to_retry: Vec<TestCase> = generated_tests::get_all_tests()
            .into_iter()
            .filter(|t| failed_names.contains(&t.name.as_str()))
            .collect();

        for attempt in run_tests_parallel(tests_to_retry, base_config.clone(), jobs, true) {
            if let Some(result) = results.iter_mut().find(|r| r.name == attempt.name) {
                result.attempts.push(attempt);
            }
        }
    }

    results
}

/// Runs multiple tests in parallel using subprocess execution.
///
/// Each test is spawned as a separate subprocess, enabling true OS-level parallelism.
/// Stdout/stderr from each subprocess is captured automatically and associated with
/// the test result.
fn run_tests_parallel(
    tests: Vec<TestCase>,
    base_config: BaseConfig,
    jobs: usize,
    is_retry: bool,
) -> Vec<AttemptResult> {
    let total_tests = tests.len();
    let current_exe = std::env::current_exe().expect("Failed to get current executable");

    println!();
    let run_type = if is_retry { "Retrying" } else { "Starting" };
    println!("{run_type} {total_tests} tests across {jobs} workers");
    if !is_retry {
        println!("─────────────────────────────────────────────────────────");
        println!("  RPC endpoint: {}", base_config.rpc_endpoint);
        println!(
            "  Prover:       {}",
            base_config.prover_endpoint.as_deref().unwrap_or("localhost")
        );
        println!(
            "  Transport:    {}",
            base_config
                .note_transport_endpoint
                .as_ref()
                .map(|e| e.to_string())
                .as_deref()
                .unwrap_or("none")
        );
        println!("  Timeout:      {}ms", base_config.timeout);
        println!("─────────────────────────────────────────────────────────");
    }
    println!();

    // Convert tests to (name, category) pairs for subprocess spawning
    let test_info: Vec<(String, String)> = tests
        .iter()
        .map(|t| (t.name.clone(), t.category.as_ref().to_string()))
        .collect();

    let results = Arc::new(Mutex::new(Vec::new()));
    let completed_count = Arc::new(AtomicUsize::new(0));

    // Use Arc<Mutex<>> to share the work queue
    let work_queue = Arc::new(Mutex::new(test_info));

    // Mutex for serializing output to prevent interleaved lines
    let output_mutex = Arc::new(Mutex::new(()));

    // Get network endpoint string for passing to subprocess
    let network_endpoint = base_config.rpc_endpoint.to_string();
    let prover_endpoint = base_config.prover_endpoint.clone();
    let note_transport_endpoint = base_config.note_transport_endpoint.clone();
    let timeout = base_config.timeout;
    let verbose = base_config.verbose;

    // Spawn worker threads (each spawns subprocesses)
    let mut handles = Vec::new();
    for _worker_id in 0..jobs {
        let work_queue = Arc::clone(&work_queue);
        let results = Arc::clone(&results);
        let completed_count = Arc::clone(&completed_count);
        let output_mutex = Arc::clone(&output_mutex);
        let current_exe = current_exe.clone();
        let network_endpoint = network_endpoint.clone();
        let prover_endpoint = prover_endpoint.clone();
        let note_transport_endpoint = note_transport_endpoint.clone();

        let handle = thread::spawn(move || {
            loop {
                // Get the next test to run
                let test = {
                    let mut queue = work_queue.lock().unwrap();
                    queue.pop()
                };

                let Some((test_name, test_category)) = test else {
                    break; // No more work
                };

                // Print "START" message
                {
                    let _lock = output_mutex.lock().unwrap();
                    println!("        START  {}::{}", test_category, test_name);
                }

                // Spawn subprocess for this test
                let mut cmd = Command::new(&current_exe);
                cmd.arg("--internal-run-test")
                    .arg(&test_name)
                    .arg("--network")
                    .arg(&network_endpoint)
                    .arg("--timeout")
                    .arg(timeout.to_string());

                // Forward prover URL if set
                if let Some(ref prover_url) = prover_endpoint {
                    cmd.arg("--prover-url").arg(prover_url);
                }

                // Forward note transport URL if set
                if let Some(ref transport) = note_transport_endpoint {
                    cmd.arg("--note-transport-url").arg(transport.to_url());
                }

                // Forward verbosity flag
                if verbose {
                    cmd.arg("--verbose");
                }

                let output = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).output();

                let progress = completed_count.fetch_add(1, Ordering::SeqCst) + 1;

                let result = match output {
                    Ok(output) => {
                        let stdout = String::from_utf8_lossy(&output.stdout);
                        let stderr = String::from_utf8_lossy(&output.stderr);

                        // Parse the JSON result from the last line of stdout
                        let subprocess_result: Option<SubprocessResult> =
                            stdout.lines().last().and_then(|line| serde_json::from_str(line).ok());

                        // Captured output is everything except the last JSON line
                        let stdout_lines: Vec<&str> = stdout.lines().collect();
                        let captured_stdout: String = if stdout_lines.len() > 1 {
                            stdout_lines[..stdout_lines.len() - 1].join("\n")
                        } else {
                            String::new()
                        };

                        let captured_output =
                            if captured_stdout.trim().is_empty() && stderr.trim().is_empty() {
                                None
                            } else if captured_stdout.trim().is_empty() {
                                Some(stderr.to_string())
                            } else if stderr.trim().is_empty() {
                                Some(captured_stdout)
                            } else {
                                Some(format!("{}\n{}", captured_stdout, stderr))
                            };

                        match subprocess_result {
                            Some(sr) => AttemptResult {
                                name: sr.name,
                                category: sr.category,
                                passed: sr.passed,
                                duration: Duration::from_secs_f64(sr.duration_secs),
                                error_message: sr.error_message,
                                captured_output,
                            },
                            None => AttemptResult {
                                name: test_name.clone(),
                                category: test_category.clone(),
                                passed: false,
                                duration: Duration::ZERO,
                                error_message: Some(format!(
                                    "Failed to parse subprocess output: {}",
                                    stdout
                                )),
                                captured_output: Some(stderr.to_string()),
                            },
                        }
                    },
                    Err(e) => AttemptResult {
                        name: test_name.clone(),
                        category: test_category.clone(),
                        passed: false,
                        duration: Duration::ZERO,
                        error_message: Some(format!("Failed to spawn subprocess: {}", e)),
                        captured_output: None,
                    },
                };

                // Print result
                {
                    let _lock = output_mutex.lock().unwrap();

                    let status = if result.passed { "PASS" } else { "FAIL" };
                    let retry_marker = if is_retry { " (retry)" } else { "" };
                    let duration_str = format_duration(result.duration);

                    println!(
                        "[{:>3}/{:>3}] {:>4}{}  {:>8}  {}::{}",
                        progress,
                        total_tests,
                        status,
                        retry_marker,
                        duration_str,
                        result.category,
                        result.name,
                    );

                    if !result.passed
                        && let Some(ref error) = result.error_message
                    {
                        println!("            Error: {error}");
                    }

                    // Show captured output in verbose mode for all tests, or
                    // inline for failures
                    if (verbose || !result.passed)
                        && let Some(ref output) = result.captured_output
                        && !output.trim().is_empty()
                    {
                        for line in output.lines() {
                            println!("            {line}");
                        }
                    }
                }

                results.lock().unwrap().push(result);
            }
        });

        handles.push(handle);
    }

    // Wait for all workers to complete
    for handle in handles {
        handle.join().unwrap();
    }

    Arc::try_unwrap(results).unwrap().into_inner().unwrap()
}

/// Formats a duration in a human-readable way, similar to nextest.
fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs_f64();
    if secs >= 60.0 {
        let mins = (secs / 60.0).floor() as u64;
        let remaining_secs = secs - (mins as f64 * 60.0);
        format!("{}m {:.1}s", mins, remaining_secs)
    } else if secs >= 1.0 {
        format!("{:.2}s", secs)
    } else {
        format!("{}ms", duration.as_millis())
    }
}

/// Prints a comprehensive summary of test execution results.
///
/// Shows pass/fail counts, failed test details, retry statistics, and timing statistics including
/// average, median, min, and max execution times.
fn print_summary(results: &[TestResult], total_duration: Duration) {
    let passed_count = results.iter().filter(|r| r.passed()).count();
    let failed_count = results.iter().filter(|r| r.failed()).count();
    let flaky_tests: Vec<_> = results.iter().filter(|r| r.is_flaky()).collect();
    let has_retries = results.iter().any(|r| r.retries() > 0);

    println!();
    println!("─────────────────────────────────────────────────────────");
    println!("  Summary");
    println!("─────────────────────────────────────────────────────────");

    if has_retries {
        println!(
            "  {} passed, {} failed, {} flaky in {}",
            passed_count,
            failed_count,
            flaky_tests.len(),
            format_duration(total_duration)
        );
    } else {
        println!(
            "  {} passed, {} failed in {}",
            passed_count,
            failed_count,
            format_duration(total_duration)
        );
    }

    if !flaky_tests.is_empty() {
        println!();
        println!("  Flaky tests (passed on retry):");
        for result in &flaky_tests {
            println!("    {}::{}", result.category, result.name);
        }
    }

    let failures: Vec<_> = results.iter().filter(|r| r.failed()).collect();
    if !failures.is_empty() {
        println!();
        println!("  Failures:");
        for result in &failures {
            println!("    FAIL {}::{}", result.category, result.name);
            if let Some(error) = result.error_message() {
                println!("         Error: {error}");
            }
            // Show captured output for failed tests
            if let Some(output) = result.captured_output() {
                println!("         ─── Captured stdout ───");
                for line in output.lines() {
                    println!("         {line}");
                }
                println!("         ─── End stdout ───");
            }
        }
    }

    // Print timing statistics
    if results.len() > 1 {
        let mut durations: Vec<_> = results.iter().map(|r| r.duration()).collect();
        durations.sort();

        let avg_duration = durations.iter().sum::<Duration>() / durations.len() as u32;
        let median_duration = durations[durations.len() / 2];
        let slowest = results.iter().max_by_key(|r| r.duration()).unwrap();

        println!();
        println!(
            "  Timing: avg {} | median {} | slowest {} ({}::{})",
            format_duration(avg_duration),
            format_duration(median_duration),
            format_duration(slowest.duration()),
            slowest.category,
            slowest.name,
        );
    }

    println!("─────────────────────────────────────────────────────────");
}

// NETWORK
// ================================================================================================

/// Represents the network to which the client connects. It is used to determine the RPC endpoint
/// and network ID for the CLI.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum Network {
    Custom(String),
    Devnet,
    Localhost,
    Testnet,
}

impl FromStr for Network {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "devnet" => Ok(Network::Devnet),
            "localhost" => Ok(Network::Localhost),
            "testnet" => Ok(Network::Testnet),
            custom => Ok(Network::Custom(custom.to_string())),
        }
    }
}

impl Network {
    /// Converts the Network variant to its corresponding RPC endpoint string
    #[allow(dead_code)]
    pub fn to_rpc_endpoint(&self) -> String {
        match self {
            Network::Custom(custom) => custom.clone(),
            Network::Devnet => Endpoint::devnet().to_string(),
            Network::Localhost => Endpoint::default().to_string(),
            Network::Testnet => Endpoint::testnet().to_string(),
        }
    }
}
