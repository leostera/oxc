use std::{
    env,
    ffi::OsStr,
    fmt::Write as _,
    io::BufWriter,
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

use oxc_diagnostics::{DiagnosticService, GraphicalTheme};

use super::{
    cache::{RunShape, Session},
    command::{FormatCommand, Mode, OutputMode},
    reporter::DefaultReporter,
    result::CliRunResult,
    service::{FormatService, SuccessResult},
    walk::{ScopedWalker, resolve_format_plan},
};
use crate::core::{
    ConfigScopes, FormatPlan, SourceFormatter, config_discovery, resolve_ignore_paths, utils,
};
#[cfg(feature = "napi")]
use crate::core::{JsConfigLoaderCb, JsDependenciesCb};

pub struct WalkRunner {
    options: FormatCommand,
    cwd: PathBuf,
    #[cfg(feature = "napi")]
    external_services: Option<crate::core::ExternalServices>,
    #[cfg(feature = "napi")]
    js_config_loader: Option<JsConfigLoaderCb>,
    /// Asks Node what it read, for `--cache`.
    #[cfg(feature = "napi")]
    js_dependencies: Option<JsDependenciesCb>,
}

impl WalkRunner {
    /// Creates a new WalkRunner instance.
    ///
    /// # Panics
    /// Panics if the current working directory cannot be determined.
    pub fn new(options: FormatCommand) -> Self {
        Self {
            options,
            cwd: env::current_dir().expect("Failed to get current working directory"),
            #[cfg(feature = "napi")]
            external_services: None,
            #[cfg(feature = "napi")]
            js_config_loader: None,
            #[cfg(feature = "napi")]
            js_dependencies: None,
        }
    }

    #[cfg(feature = "napi")]
    #[must_use]
    pub fn with_external_services(
        mut self,
        external_services: Option<crate::core::ExternalServices>,
    ) -> Self {
        self.external_services = external_services;
        self
    }

    #[cfg(feature = "napi")]
    #[must_use]
    pub fn with_js_config_loader(mut self, js_config_loader: JsConfigLoaderCb) -> Self {
        self.js_config_loader = Some(js_config_loader);
        self
    }

    /// What Node read, for configs or in the format workers: for `--cache`.
    #[cfg(feature = "napi")]
    #[must_use]
    pub fn with_js_dependencies(mut self, js_dependencies: JsDependenciesCb) -> Self {
        self.js_dependencies = Some(js_dependencies);
        self
    }

    /// # Panics
    /// Panics if `napi` feature is enabled but external_services is not set.
    pub fn run(self) -> CliRunResult {
        // stdio is blocked by `LineWriter`, use a `BufWriter` to reduce syscalls.
        // See https://github.com/rust-lang/rust/issues/60673
        let stdout = &mut BufWriter::new(std::io::stdout());
        let stderr = &mut BufWriter::new(std::io::stderr());

        let start_time = Instant::now();

        let cwd = self.cwd;
        let FormatCommand {
            paths,
            mode,
            config_options,
            ignore_options,
            runtime_options,
            cache_options,
        } = self.options;
        // If `napi` feature is disabled, there is no other mode.
        #[cfg_attr(not(feature = "napi"), expect(irrefutable_let_patterns))]
        let Mode::Cli(format_mode) = mode else {
            unreachable!("`WalkRunner` should only be called with Mode::Cli");
        };
        let num_of_threads = rayon::current_num_threads();

        // With `--cache`: the last run of the same shape, and what of it still holds.
        let vp_version = oxc_config::vp_version();
        let mut cache = cache_options
            .cache
            .then(|| {
                let shape = RunShape {
                    cwd: &cwd,
                    paths: &paths,
                    config: config_options.config.as_deref(),
                    use_nested_configs: config_options.use_nested_configs(),
                    disable_nested_config: config_options.disable_nested_config,
                    ignore_paths: &ignore_options.ignore_path,
                    with_node_modules: ignore_options.with_node_modules,
                    vp_version: vp_version.as_deref().map(OsStr::as_encoded_bytes),
                };
                Session::open(cache_options.cache_location.as_deref(), &shape)
            })
            .flatten();
        #[cfg(feature = "napi")]
        if let (Some(cache), Some(js_dependencies)) = (&mut cache, self.js_dependencies) {
            cache.set_js_dependencies(js_dependencies);
        }
        #[cfg(feature = "napi")]
        let js_config_loader = match &cache {
            Some(cache) => self.js_config_loader.map(|loader| cache.js_config_loader(loader)),
            None => self.js_config_loader,
        };
        // Nothing the walk or the configs read changed since the last run: no walk, and the
        // configs are loaded only if a file needs formatting.
        let replay = cache.as_ref().and_then(Session::replay);

        // Find and load root config file, and set up the external services
        let load_config_scopes = |stderr: &mut BufWriter<std::io::Stderr>| {
            let config_scopes = match ConfigScopes::load(
                &cwd,
                config_options.config.as_deref(),
                config_options.use_nested_configs(),
                #[cfg(feature = "napi")]
                js_config_loader.as_ref(),
            ) {
                Ok(scopes) => scopes,
                Err(err) => {
                    utils::print_and_flush(stderr, &format!("{err}\n"));
                    return None;
                }
            };

            // NOTE: The caller (`run_cli()`) runs this whole function in `block_in_place()`
            #[cfg(feature = "napi")]
            if let Err(err) = self
                .external_services
                .as_ref()
                .expect("External services must be set when `napi` feature is enabled")
                .init(num_of_threads)
            {
                utils::print_and_flush(
                    stderr,
                    &format!("Failed to setup external services.\n{err}\n"),
                );
                return None;
            }
            Some(config_scopes)
        };
        let config_scopes = match &replay {
            Some(_) => None,
            None => match load_config_scopes(stderr) {
                Some(config_scopes) => Some(config_scopes),
                None => return CliRunResult::InvalidOptionConfig,
            },
        };

        // Resolve ignore paths early to validate before walk starts
        let resolved_ignore_paths = match resolve_ignore_paths(&cwd, &ignore_options.ignore_path) {
            Ok(paths) => paths,
            Err(err) => {
                utils::print_and_flush(
                    stderr,
                    &format!("Failed to parse target paths or ignore settings.\n{err}\n"),
                );
                return CliRunResult::InvalidOptionConfig;
            }
        };

        // Shared channel for format entries from all scopes
        let (tx_entry, rx_entry) = mpsc::channel::<FormatPlan>();
        // Collect format results (changed paths or unchanged count)
        let (tx_success, rx_success) = mpsc::channel();
        // Diagnostic from formatting service
        let (mut diagnostic_service, tx_error) =
            DiagnosticService::new(Box::new(DefaultReporter::default()));

        if matches!(format_mode, OutputMode::Check) {
            utils::print_and_flush(stdout, "Checking formatting...\n");
            utils::print_and_flush(stdout, "\n");
        }

        // Create `SourceFormatter` instance
        let source_formatter = SourceFormatter::new(num_of_threads);
        #[cfg(feature = "napi")]
        let source_formatter =
            source_formatter.with_external_services(self.external_services.clone());

        let cwd_for_format = cwd.clone();
        // Clone `tx_error` so both the walk threads and the format service can report errors
        let tx_error_for_format = tx_error.clone();
        let files = cache.as_ref().map(Session::files);

        // Spawn formatting service on a dedicated thread so it doesn't occupy the rayon pool.
        // It just blocks on `rx_entry` waiting for entries; `par_bridge()` inside still uses rayon.
        let format_thread = std::thread::spawn(move || {
            let format_service =
                FormatService::new(cwd_for_format, format_mode, source_formatter).with_cache(files);
            format_service.run_streaming(rx_entry, &tx_error_for_format, &tx_success);
        });

        let mut unchanged_count: usize = 0;
        let any_config_found = if let Some(replay) = replay {
            // The last run's files: those whose stat says they're formatted are counted,
            // the others resolved and formatted.
            unchanged_count += replay.formatted;
            if !replay.stale.is_empty() {
                let Some(config_scopes) = load_config_scopes(stderr) else {
                    return CliRunResult::InvalidOptionConfig;
                };
                for path in replay.stale {
                    let resolver = match config_scopes.resolve(&path) {
                        Ok(resolver) => resolver,
                        Err(err) => {
                            utils::print_and_flush(
                                stderr,
                                &format!("Failed to parse configuration.\n{err}\n"),
                            );
                            return CliRunResult::InvalidOptionConfig;
                        }
                    };
                    if let Some(plan) = resolve_format_plan(&path, &resolver, &tx_error, &cwd, None)
                    {
                        let _ = tx_entry.send(plan);
                    }
                }
            }
            drop(tx_entry);
            drop(tx_error);
            if let Some(cache) = &mut cache {
                cache.replayed();
            }
            replay.any_config_found
        } else {
            let config_scopes = config_scopes.expect("loaded unless the walk is replayed");
            let recorder = cache.as_ref().map(|cache| {
                cache.recorder(config_discovery().config_file_names(), config_scopes.use_nested())
            });
            if let Some(recorder) = &recorder {
                // What the run reads outside the walk: an explicit config, and ignore files.
                let config = config_options
                    .config
                    .as_deref()
                    .map(|config| utils::normalize_relative_path(&cwd, config));
                let default_ignore =
                    ignore_options.ignore_path.is_empty().then(|| cwd.join(".prettierignore"));
                for path in config.iter().chain(&default_ignore).chain(&resolved_ignore_paths) {
                    recorder.file(path);
                }
            }

            // Run scoped walks (root + nested) sends entries to `tx_entry` and errors to `tx_error`.
            // Manually drop after the walk to signal the formatting service that no more entries will be sent.
            let walker = match ScopedWalker::new(cwd.clone(), &paths) {
                Ok(walker) => walker.with_recorder(recorder.clone()),
                Err(err) => {
                    drop(tx_entry);
                    drop(tx_error);
                    utils::print_and_flush(stderr, &format!("{err}\n"));
                    return CliRunResult::InvalidOptionConfig;
                }
            };
            let walked = walker.run(
                &config_scopes,
                &resolved_ignore_paths,
                ignore_options.with_node_modules,
                &tx_entry,
                &tx_error,
            );
            drop(walker);
            drop(tx_entry);
            drop(tx_error);
            if let Err(err) = walked {
                let mut msg = format!("Failed to parse configuration.\n{err}\n");
                // Nested configs are loaded lazily during the walk,
                // so by the time a load failure surfaces some files in unaffected scopes may already have been written.
                if matches!(format_mode, OutputMode::Write) {
                    msg.push_str(
                        "Files in unaffected scopes may have already been written; re-run after fixing the config.\n",
                    );
                }
                utils::print_and_flush(stderr, &msg);
                return CliRunResult::InvalidOptionConfig;
            }
            let any_config_found = config_scopes.any_config_found();
            if let (Some(cache), Some(recorder)) = (&mut cache, recorder) {
                cache.walked(recorder, any_config_found);
            }
            any_config_found
        };

        // Collect results and separate changed paths from unchanged count
        let mut changed_paths: Vec<(String, Option<Duration>)> = vec![];
        for result in rx_success {
            match result {
                SuccessResult::Changed(path, elapsed) => changed_paths.push((path, elapsed)),
                SuccessResult::Unchanged => unchanged_count += 1,
            }
        }
        // Once the service is done with the file cache.
        if let Some(cache) = cache {
            let _ = format_thread.join();
            cache.save();
        }

        // Print sorted changed file paths to stdout
        let changed_count = changed_paths.len();
        if changed_count != 0 {
            changed_paths.sort_unstable();
            // Colorize paths in `Check` mode:
            // Make them stand out from surrounding status lines and align with error diagnostics.
            // Color support is detected the same way as `GraphicalReportHandler` used by `DefaultReporter`,
            // and when colors are disabled, every style is empty and renders nothing.
            // `ListDifferent` mode output is meant for piping, keep it style-free.
            let warning_theme =
                matches!(format_mode, OutputMode::Check).then(GraphicalTheme::default);
            let mut output = String::new();
            for (path, elapsed) in changed_paths {
                match &warning_theme {
                    // Style per line, some log viewers reset ANSI state on line breaks
                    Some(theme) => theme.write_warning(&mut output, &path).unwrap(),
                    None => output.push_str(&path),
                }
                if let Some(elapsed) = elapsed {
                    write!(output, " ({}ms)", elapsed.as_millis()).unwrap();
                }
                output.push('\n');
            }
            utils::print_and_flush(stdout, &output);
        }

        // Then, output diagnostics errors to stderr
        // NOTE: This is blocking and print errors
        let diagnostics = diagnostic_service.run(stderr);
        // NOTE: We are not using `DiagnosticService` for warnings
        let error_count = diagnostics.errors_count();

        // Count the processed files
        let total_target_files_count = changed_count + unchanged_count + error_count;
        let print_stats = |stdout, stderr| {
            utils::print_and_flush(
                stdout,
                &format!(
                    "Finished in {}ms on {total_target_files_count} files using {num_of_threads} threads.\n",
                    start_time.elapsed().as_millis()
                ),
            );
            // Config stats: only show when no config is found
            if !any_config_found {
                #[cfg(feature = "napi")]
                let hint = "No config found, using defaults. Please add a config file or try `oxfmt --init` if needed.\n";
                #[cfg(not(feature = "napi"))]
                let hint =
                    "No config found, using defaults. Please add `.oxfmtrc.json` if needed.\n";
                utils::print_and_flush(stderr, hint);
            }
        };

        // Check if no files were found
        if total_target_files_count == 0 {
            if runtime_options.no_error_on_unmatched_pattern {
                utils::print_and_flush(stderr, "No files found matching the given patterns.\n");
                print_stats(stdout, stderr);
                return CliRunResult::None;
            }

            utils::print_and_flush(
                stderr,
                "Expected at least one target file. All matched files may have been excluded by ignore rules.\n",
            );
            return CliRunResult::NoFilesFound;
        }

        if 0 < error_count {
            // Each error is already printed in reporter
            utils::print_and_flush(
                stderr,
                "Error occurred when checking code style in the above files.\n",
            );
            return CliRunResult::FormatFailed;
        }

        match (&format_mode, changed_count) {
            // `--list-different` outputs nothing here, mismatched paths are already printed to stdout
            (OutputMode::ListDifferent, 0) => CliRunResult::FormatSucceeded,
            (OutputMode::ListDifferent, _) => CliRunResult::FormatMismatch,
            // `--check` outputs friendly summary
            (OutputMode::Check, 0) => {
                utils::print_and_flush(stdout, "All matched files use the correct format.\n");
                print_stats(stdout, stderr);
                CliRunResult::FormatSucceeded
            }
            (OutputMode::Check, _) => {
                utils::print_and_flush(stdout, "\n");
                utils::print_and_flush(
                    stdout,
                    &format!(
                        "Format issues found in above {changed_count} files. Run without `--check` to fix.\n",
                    ),
                );
                print_stats(stdout, stderr);
                CliRunResult::FormatMismatch
            }
            // Default (write) outputs only stats
            (OutputMode::Write, _) => {
                // Each changed file is also NOT printed
                debug_assert_eq!(
                    changed_count, 0,
                    "In write mode, changed_count should not be counted"
                );
                print_stats(stdout, stderr);
                CliRunResult::FormatSucceeded
            }
        }
    }
}
