#![forbid(unsafe_code)]

// TODO: Investigate how cargo-clippy is implemented. Is it using syn?
// Is is using rustc? Is it implementing a compiler plugin?

// TODO: Add a new output format that adds all unsafe usage counts to a single
// number?
//     10 / 10     crate-one-0.1.0
//     5  / 123    some-other-crate-0.1.0
//     0  / 456    and-another-one-0.1.0

#[macro_use]
extern crate structopt;
extern crate cargo;
extern crate colored;
extern crate env_logger;
extern crate failure;
extern crate petgraph;
extern crate syn;
extern crate walkdir;

use self::walkdir::DirEntry;
use self::walkdir::WalkDir;
use cargo::core::compiler::CompileMode;
use cargo::core::compiler::Executor;
use cargo::core::compiler::Unit;
use cargo::core::dependency::Kind;
use cargo::core::package::PackageSet;
use cargo::core::registry::PackageRegistry;
use cargo::core::resolver::Method;
use cargo::core::shell::Shell;
use cargo::core::shell::Verbosity;
use cargo::core::Target;
use cargo::core::{Package, PackageId, Resolve, Workspace};
use cargo::ops;
use cargo::ops::CleanOptions;
use cargo::ops::CompileOptions;
use cargo::util::paths;
use cargo::util::ProcessBuilder;
use cargo::util::{self, important_paths, CargoResult, Cfg};
use cargo::{CliResult, Config};
use colored::*;
use format::Pattern;
use petgraph::graph::NodeIndex;
use petgraph::visit::EdgeRef;
use petgraph::EdgeDirection;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::str::{self, FromStr};
use std::sync::Arc;
use std::sync::Mutex;
use structopt::clap::AppSettings;
use structopt::StructOpt;
use syn::{visit, Expr, ImplItemMethod, ItemFn, ItemImpl, ItemMod, ItemTrait};

mod format;

#[derive(Debug, Default)]
pub struct Count {
    /// Number of safe items, in .rs files not used by the build.
    pub safe_unused: u64,

    /// Number of safe items, in .rs files used by the build.
    pub safe_used: u64,

    /// Number of unsafe items, in .rs files not used by the build.
    pub unsafe_unused: u64,

    /// Number of unsafe items, in .rs files used by the build.
    pub unsafe_used: u64,
}

impl Count {
    fn count(&mut self, is_unsafe: bool, used_by_build: bool) {
        match (is_unsafe, used_by_build) {
            (false, false) => self.safe_unused += 1,
            (false, true) => self.safe_used += 1,
            (true, false) => self.unsafe_unused += 1,
            (true, true) => self.unsafe_used += 1,
        }
    }
}

/// Unsafe usage metrics collection.
#[derive(Debug, Default)]
struct CounterBlock {
    pub functions: Count,
    pub exprs: Count,
    pub itemimpls: Count,
    pub itemtraits: Count,
    pub methods: Count,
}

impl CounterBlock {
    fn has_unsafe(&self) -> bool {
        self.functions.unsafe_used > 0
            || self.exprs.unsafe_used > 0
            || self.itemimpls.unsafe_used > 0
            || self.itemtraits.unsafe_used > 0
            || self.methods.unsafe_used > 0
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum IncludeTests {
    Yes,
    No,
}

struct GeigerSynVisitor {
    /// Count unsafe usage inside tests
    pub include_tests: IncludeTests,

    /// Verbose logging.
    pub verbosity: Verbosity,

    /// Metrics storage.
    pub counters: CounterBlock,

    /// Used by the Visit trait implementation to separate the metrics into
    /// "used by build" and "not used by build" based on if the .rs file was
    /// used in the build or not.
    pub used_by_build: bool,

    /// Used by the Visit trait implementation to track the traversal state.
    in_unsafe_block: bool,
}

impl GeigerSynVisitor {
    pub fn new(include_tests: IncludeTests, verbosity: Verbosity) -> Self {
        GeigerSynVisitor {
            include_tests,
            verbosity,
            counters: Default::default(),
            used_by_build: false,
            in_unsafe_block: false,
        }
    }
}

/// Will return true for #[cfg(test)] decodated modules.
///
/// This function is a somewhat of a hack and will probably missinterpret more
/// advanded cfg expressions. A better way to do this would be to let rustc emit
/// every single source file path and span within each source file and use that
/// as a general filter for included code.
/// TODO: Investigate if the needed information can be emitted by rustc today.
fn is_test_mod(i: &ItemMod) -> bool {
    use syn::Meta;
    i.attrs
        .iter()
        .flat_map(|a| a.interpret_meta())
        .any(|m| match m {
            Meta::List(ml) => meta_list_is_cfg_test(&ml),
            _ => false,
        })
}

// MetaList {
//     ident: Ident(
//         cfg
//     ),
//     paren_token: Paren,
//     nested: [
//         Meta(
//             Word(
//                 Ident(
//                     test
//                 )
//             )
//         )
//     ]
// }
fn meta_list_is_cfg_test(ml: &syn::MetaList) -> bool {
    use syn::NestedMeta;
    if ml.ident != "cfg" {
        return false;
    }
    ml.nested.iter().any(|n| match n {
        NestedMeta::Meta(meta) => meta_is_word_test(meta),
        _ => false,
    })
}

fn meta_is_word_test(m: &syn::Meta) -> bool {
    use syn::Meta;
    match m {
        Meta::Word(ident) => ident == "test",
        _ => false,
    }
}

fn is_test_fn(i: &ItemFn) -> bool {
    i.attrs
        .iter()
        .flat_map(|a| a.interpret_meta())
        .any(|m| meta_is_word_test(&m))
}

impl<'ast> visit::Visit<'ast> for GeigerSynVisitor {
    /// Free-standing functions
    fn visit_item_fn(&mut self, i: &ItemFn) {
        if IncludeTests::No == self.include_tests && is_test_fn(i) {
            return;
        }
        self.counters
            .functions
            .count(i.unsafety.is_some(), self.used_by_build);
        visit::visit_item_fn(self, i);
    }

    fn visit_expr(&mut self, i: &Expr) {
        // Total number of expressions of any type
        match i {
            Expr::Unsafe(i) => {
                self.in_unsafe_block = true;
                visit::visit_expr_unsafe(self, i);
                self.in_unsafe_block = false;
            }
            Expr::Path(_) | Expr::Lit(_) => {
                // Do not count. The expression `f(x)` should count as one
                // expression, not three.
            }
            other => {
                // TODO: Print something pretty here or gather the data for later
                // printing.
                // if self.verbosity == Verbosity::Verbose && self.in_unsafe_block {
                //     println!("{:#?}", other);
                // }
                self.counters
                    .exprs
                    .count(self.in_unsafe_block, self.used_by_build);
                visit::visit_expr(self, other);
            }
        }
    }

    fn visit_item_mod(&mut self, i: &ItemMod) {
        if IncludeTests::No == self.include_tests && is_test_mod(i) {
            return;
        }
        visit::visit_item_mod(self, i);
    }

    fn visit_item_impl(&mut self, i: &ItemImpl) {
        // unsafe trait impl's
        self.counters
            .itemimpls
            .count(i.unsafety.is_some(), self.used_by_build);
        visit::visit_item_impl(self, i);
    }

    fn visit_item_trait(&mut self, i: &ItemTrait) {
        // Unsafe traits
        self.counters
            .itemtraits
            .count(i.unsafety.is_some(), self.used_by_build);
        visit::visit_item_trait(self, i);
    }

    fn visit_impl_item_method(&mut self, i: &ImplItemMethod) {
        self.counters
            .methods
            .count(i.sig.unsafety.is_some(), self.used_by_build);
        visit::visit_impl_item_method(self, i);
    }
}

fn is_file_with_ext(entry: &DirEntry, file_ext: &str) -> bool {
    if !entry.file_type().is_file() {
        return false;
    }
    let p = entry.path();
    let ext = match p.extension() {
        Some(e) => e,
        None => return false,
    };
    // to_string_lossy is ok since we only want to match against an ASCII
    // compatible extension and we do not keep the possibly lossy result
    // around.
    ext.to_string_lossy() == file_ext
}

fn find_unsafe(
    p: &Path,
    rs_files_used: &mut HashMap<PathBuf, u32>,
    allow_partial_results: bool,
    include_tests: IncludeTests,
    verbosity: Verbosity,
) -> CounterBlock {
    let mut vis = GeigerSynVisitor::new(include_tests, verbosity);
    let walker = WalkDir::new(p).into_iter();
    for entry in walker {
        let entry =
            entry.expect("walkdir error, TODO: Implement error handling");
        if !is_file_with_ext(&entry, "rs") {
            continue;
        }
        let p = entry.path();
        let scan_counter = rs_files_used.get_mut(p);
        vis.used_by_build = match scan_counter {
            Some(c) => {
                // TODO: Add proper logging.
                if verbosity == Verbosity::Verbose {
                    println!("Used in build: {}", p.display());
                }
                // This .rs file path was found by intercepting rustc arguments
                // or by parsing the .d files produced by rustc. Here we
                // increase the counter for this path to mark that this file
                // has been scanned. Warnings will be printed for .rs files in
                // this collection with a count of 0 (has not been scanned). If
                // this happens, it could indicate a logic error or some
                // incorrect assumption in cargo-geiger.
                *c += 1;
                true
            }
            None => {
                // This file was not used in the build triggered by
                // cargo-geiger, but it should be scanned anyways to provide
                // both "in build" and "not in build" stats.
                // TODO: Add proper logging.
                if verbosity == Verbosity::Verbose {
                    println!("Not used in build: {}", p.display());
                }
                false
            }
        };
        let mut file = File::open(p).expect("Unable to open file");
        let mut src = vec![];
        file.read_to_end(&mut src).expect("Unable to read file");
        let syntax = match (allow_partial_results, syn::parse_file(&String::from_utf8_lossy(&src))) {
            (_, Ok(s)) => s,
            (true, Err(e)) => {
                // TODO: Do proper error logging.
                println!("Failed to parse file: {}, {:?}", p.display(), e);
                continue;
            }
            (false, Err(e)) => {
                panic!("Failed to parse file: {}, {:?} ", p.display(), e)
            }
        };
        syn::visit::visit_file(&mut vis, &syntax);
    }
    vis.counters
}

#[derive(StructOpt)]
#[structopt(bin_name = "cargo")]
enum Opts {
    #[structopt(
        name = "geiger",
        raw(
            setting = "AppSettings::UnifiedHelpMessage",
            setting = "AppSettings::DeriveDisplayOrder",
            setting = "AppSettings::DontCollapseArgsInUsage"
        )
    )]
    /// Detects usage of unsafe Rust in a Rust crate and its dependencies.
    Geiger(Args),
}

#[derive(StructOpt)]
struct Args {
    #[structopt(long = "package", short = "p", value_name = "SPEC")]
    /// Package to be used as the root of the tree
    package: Option<String>,

    #[structopt(long = "features", value_name = "FEATURES")]
    /// Space-separated list of features to activate
    features: Option<String>,

    #[structopt(long = "all-features")]
    /// Activate all available features
    all_features: bool,

    #[structopt(long = "no-default-features")]
    /// Do not activate the `default` feature
    no_default_features: bool,

    #[structopt(long = "target", value_name = "TARGET")]
    /// Set the target triple
    target: Option<String>,

    #[structopt(long = "all-targets")]
    /// Return dependencies for all targets. By default only the host target is matched.
    all_targets: bool,

    #[structopt(
        long = "manifest-path",
        value_name = "PATH",
        parse(from_os_str)
    )]
    /// Path to Cargo.toml
    manifest_path: Option<PathBuf>,

    #[structopt(long = "invert", short = "i")]
    /// Invert the tree direction
    invert: bool,

    #[structopt(long = "no-indent")]
    /// Display the dependencies as a list (rather than a tree)
    no_indent: bool,

    #[structopt(long = "prefix-depth")]
    /// Display the dependencies as a list (rather than a tree), but prefixed with the depth
    prefix_depth: bool,

    #[structopt(long = "all", short = "a")]
    /// Don't truncate dependencies that have already been displayed
    all: bool,

    #[structopt(
        long = "charset",
        value_name = "CHARSET",
        default_value = "utf8"
    )]
    /// Character set to use in output: utf8, ascii
    charset: Charset,

    #[structopt(
        long = "format",
        short = "f",
        value_name = "FORMAT",
        default_value = "{p}"
    )]
    /// Format string used for printing dependencies
    format: String,

    #[structopt(long = "verbose", short = "v", parse(from_occurrences))]
    /// Use verbose output (-vv very verbose/build.rs output)
    verbose: u32,

    #[structopt(long = "quiet", short = "q")]
    /// No output printed to stdout other than the tree
    quiet: Option<bool>,

    #[structopt(long = "color", value_name = "WHEN")]
    /// Coloring: auto, always, never
    color: Option<String>,

    #[structopt(long = "frozen")]
    /// Require Cargo.lock and cache are up to date
    frozen: bool,

    #[structopt(long = "locked")]
    /// Require Cargo.lock is up to date
    locked: bool,

    #[structopt(short = "Z", value_name = "FLAG")]
    /// Unstable (nightly-only) flags to Cargo
    unstable_flags: Vec<String>,

    // TODO: Implement a new compact output mode where all metrics are
    // aggregated to a single used/unused ratio and output string.
    //#[structopt(long = "compact")]
    // Display compact output instead of table
    //compact: bool,
    #[structopt(long = "include-tests")]
    /// Count unsafe usage in tests.
    include_tests: bool,
}

enum Charset {
    Utf8,
    Ascii,
}

#[derive(Clone, Copy)]
enum Prefix {
    None,
    Indent,
    Depth,
}

impl FromStr for Charset {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Charset, &'static str> {
        match s {
            "utf8" => Ok(Charset::Utf8),
            "ascii" => Ok(Charset::Ascii),
            _ => Err("invalid charset"),
        }
    }
}

struct Symbols {
    down: &'static str,
    tee: &'static str,
    ell: &'static str,
    right: &'static str,
}

static UTF8_SYMBOLS: Symbols = Symbols {
    down: "│",
    tee: "├",
    ell: "└",
    right: "─",
};

static ASCII_SYMBOLS: Symbols = Symbols {
    down: "|",
    tee: "|",
    ell: "`",
    right: "-",
};

fn main() {
    env_logger::init();

    let mut config = match Config::default() {
        Ok(cfg) => cfg,
        Err(e) => {
            let mut shell = Shell::new();
            cargo::exit_with_error(e.into(), &mut shell)
        }
    };

    let Opts::Geiger(args) = Opts::from_args();

    if let Err(e) = real_main(&args, &mut config) {
        let mut shell = Shell::new();
        cargo::exit_with_error(e, &mut shell)
    }
}

struct PrintConfig<'a> {
    /// Don't truncate dependencies that have already been displayed.
    pub all: bool,

    pub verbosity: Verbosity,
    pub direction: EdgeDirection,
    pub prefix: Prefix,
    pub format: &'a Pattern,
    pub symbols: &'a Symbols,
    pub allow_partial_results: bool,
    pub include_tests: IncludeTests,
}

fn real_main(args: &Args, config: &mut Config) -> CliResult {
    let target_dir = None; // Doesn't add any value for cargo-geiger.
    config.configure(
        args.verbose,
        args.quiet,
        &args.color,
        args.frozen,
        args.locked,
        &target_dir,
        &args.unstable_flags,
    )?;
    let verbosity = if args.verbose == 0 {
        Verbosity::Normal
    } else {
        Verbosity::Verbose
    };
    let ws = workspace(config, args.manifest_path.clone())?;
    let package = ws.current()?;
    let mut registry = registry(config, &package)?;
    let (packages, resolve) = resolve(
        &mut registry,
        &ws,
        args.features.clone(),
        args.all_features,
        args.no_default_features,
    )?;
    let ids = packages.package_ids().cloned().collect::<Vec<_>>();
    let packages = registry.get(&ids);

    let root = match args.package {
        Some(ref pkg) => resolve.query(pkg)?,
        None => package.package_id(),
    };

    let config_host = config.rustc(Some(&ws))?.host;
    let target = if args.all_targets {
        None
    } else {
        Some(args.target.as_ref().unwrap_or(&config_host).as_str())
    };

    let format = Pattern::new(&args.format)
        .map_err(|e| failure::err_msg(e.to_string()))?;

    let cfgs = get_cfgs(config, &args.target, &ws)?;
    let graph = build_graph(
        &resolve,
        &packages,
        package.package_id(),
        target,
        cfgs.as_ref().map(|r| &**r),
    )?;

    let direction = if args.invert {
        EdgeDirection::Incoming
    } else {
        EdgeDirection::Outgoing
    };

    let symbols = match args.charset {
        Charset::Ascii => &ASCII_SYMBOLS,
        Charset::Utf8 => &UTF8_SYMBOLS,
    };

    let prefix = if args.prefix_depth {
        Prefix::Depth
    } else if args.no_indent {
        Prefix::None
    } else {
        Prefix::Indent
    };

    let mut rs_files_used = resolve_rs_file_deps(&args, &ws).unwrap();

    if verbosity == Verbosity::Verbose {
        // Print all .rs files found through the .d files, in sorted order.
        let mut paths = rs_files_used
            .keys()
            .map(|k| k.to_owned())
            .collect::<Vec<PathBuf>>();
        paths.sort();
        paths
            .iter()
            .for_each(|p| println!("Used by build (sorted): {}", p.display()));
    }

    println!();
    println!("Metric output format: x/y");
    println!("x = unsafe code used by the build");
    println!("y = total unsafe code found in the crate");
    println!();
    println!(
        "{}",
        UNSAFE_COUNTERS_HEADER
            .iter()
            .map(|s| s.to_owned())
            .collect::<Vec<_>>()
            .join(" ")
            .bold()
    );
    println!();

    // TODO: Add command line flag for this and make it default to false?
    let allow_partial_results = true;

    let include_tests = if args.include_tests {
        IncludeTests::Yes
    } else {
        IncludeTests::No
    };
    let pc = PrintConfig {
        all: args.all,
        verbosity,
        direction,
        prefix,
        format: &format,
        symbols,
        allow_partial_results,
        include_tests,
    };
    print_tree(root, &graph, &mut rs_files_used, &pc);
    rs_files_used
        .iter()
        .filter(|(_k, v)| **v == 0)
        .for_each(|(k, _v)| {
            println!(
                "WARNING: Dependency file was never scanned: {}",
                k.display()
            )
        });
    Ok(())
}

/// Based on code from cargo-bloat. It seems weird that CompileOptions can be
/// constructed without providing all standard cargo options, TODO: Open an issue
/// in cargo?
fn build_compile_options<'a>(
    args: &'a Args,
    config: &'a Config,
) -> CompileOptions<'a> {
    let features = Method::split_features(
        &args.features.clone().into_iter().collect::<Vec<_>>(),
    ).into_iter()
    .map(|s| s.to_string());
    let mut opt =
        CompileOptions::new(&config, CompileMode::Check { test: false })
            .unwrap();
    opt.features = features.collect::<_>();
    opt.all_features = args.all_features;
    opt.no_default_features = args.no_default_features;

    // TODO: Investigate if this is relevant to cargo-geiger.
    //let mut bins = Vec::new();
    //let mut examples = Vec::new();
    // opt.release = args.release;
    // opt.target = args.target.clone();
    // if let Some(ref name) = args.bin {
    //     bins.push(name.clone());
    // } else if let Some(ref name) = args.example {
    //     examples.push(name.clone());
    // }
    // if args.bin.is_some() || args.example.is_some() {
    //     opt.filter = ops::CompileFilter::new(
    //         false,
    //         bins.clone(), false,
    //         Vec::new(), false,
    //         examples.clone(), false,
    //         Vec::new(), false,
    //         false,
    //     );
    // }

    opt
}

#[derive(Debug)]
enum RsResolveError {
    Walkdir(walkdir::Error),

    /// Like io::Error but with the related path.
    Io(io::Error, PathBuf),

    /// Would like cargo::Error here, but it's private, why?
    /// This is still way better than a panic though.
    Cargo(String),

    /// This should not happen unless incorrect assumptions have been made in
    /// cargo-geiger about how the cargo API works.
    ArcUnwrap(),

    /// Failed to get the inner context out of the mutex.
    InnerContextMutex(String),

    /// TODO: Add file path involved in parse error.
    DepParse(String),
}

impl Error for RsResolveError {}

impl fmt::Display for RsResolveError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl From<PoisonError<CustomExecutorInnerContext>> for RsResolveError {
    fn from(e: PoisonError<CustomExecutorInnerContext>) -> Self {
        RsResolveError::InnerContextMutex(e.to_string())
    }
}

fn resolve_rs_file_deps(
    args: &Args,
    ws: &Workspace,
) -> Result<HashMap<PathBuf, u32>, RsResolveError> {
    let config = ws.config();
    // Need to run a cargo clean to identify all new .d deps files.
    let clean_opt = CleanOptions {
        config: &config,
        spec: vec![],
        target: None,
        release: false,
        doc: false,
    };
    ops::clean(ws, &clean_opt)
        .map_err(|e| RsResolveError::Cargo(e.to_string()))?;
    let copt = build_compile_options(args, config);
    let executor = Arc::new(CustomExecutor {
        cwd: config.cwd().to_path_buf(),
        ..Default::default()
    });
    ops::compile_with_exec(ws, &copt, executor.clone())
        .map_err(|e| RsResolveError::Cargo(e.to_string()))?;
    let executor =
        Arc::try_unwrap(executor).map_err(|_| RsResolveError::ArcUnwrap())?;
    let (rs_files, out_dir_args) = {
        let inner = executor.into_inner()?;
        (inner.rs_file_args, inner.out_dir_args)
    };
    let ws_root = ws.root().to_path_buf();
    let mut hm = HashMap::<PathBuf, u32>::new();
    for out_dir in out_dir_args {
        for ent in WalkDir::new(&out_dir) {
            let ent = ent.map_err(RsResolveError::Walkdir)?;
            if !is_file_with_ext(&ent, "d") {
                continue;
            }
            let deps = parse_rustc_dep_info(ent.path())
                .map_err(|e| RsResolveError::DepParse(e.to_string()))?;
            let canon_paths = deps
                .into_iter()
                .flat_map(|t| t.1)
                .map(PathBuf::from)
                .map(|pb| ws_root.join(pb))
                .map(|pb| {
                    pb.canonicalize().map_err(|e| RsResolveError::Io(e, pb))
                });
            for p in canon_paths {
                hm.insert(p?, 0);
            }
        }
    }
    for pb in rs_files {
        // rs_files must already be canonicalized
        hm.insert(pb, 0);
    }
    Ok(hm)
}

/// Copy-pasted (almost) from the private module cargo::core::compiler::fingerprint.
///
/// TODO: Make a PR to the cargo project to expose this function or to expose
/// the dependency data in some other way.
fn parse_rustc_dep_info(
    rustc_dep_info: &Path,
) -> CargoResult<Vec<(String, Vec<String>)>> {
    let contents = paths::read(rustc_dep_info)?;
    contents
        .lines()
        .filter_map(|l| l.find(": ").map(|i| (l, i)))
        .map(|(line, pos)| {
            let target = &line[..pos];
            let mut deps = line[pos + 2..].split_whitespace();
            let mut ret = Vec::new();
            while let Some(s) = deps.next() {
                let mut file = s.to_string();
                while file.ends_with('\\') {
                    file.pop();
                    file.push(' ');
                    //file.push_str(deps.next().ok_or_else(|| {
                    //internal("malformed dep-info format, trailing \\".to_string())
                    //})?);
                    file.push_str(deps.next().expect("malformed dep-info format, trailing \\"));
                }
                ret.push(file);
            }
            Ok((target.to_string(), ret))
        })
        .collect()
}

#[derive(Debug, Default)]
struct CustomExecutorInnerContext {
    /// Stores all lib.rs, main.rs etc. passed to rustc during the build.
    pub rs_file_args: HashSet<PathBuf>,

    /// Investigate if this needs to be intercepted like this or if it can be
    /// looked up in a nicer way.
    pub out_dir_args: HashSet<PathBuf>,
}

use std::sync::PoisonError;

/// A cargo Executor to intercept all build tasks and store all ".rs" file
/// paths for later scanning.
///
/// TODO: This is the place(?) to make rustc perform macro expansion to allow
/// scanning of the the expanded code. (incl. code generated by build.rs).
/// Seems to require nightly rust.
#[derive(Debug, Default)]
struct CustomExecutor {
    /// MAJOR LIFETIME BOUNDS RAGE: Figure out how to use &Path here.
    pub cwd: PathBuf,

    /// Needed since multiple rustc calls can be in flight at the same time.
    pub inner_ctx: Mutex<CustomExecutorInnerContext>,
}

impl CustomExecutor {
    pub fn into_inner(
        self,
    ) -> Result<
        CustomExecutorInnerContext,
        PoisonError<CustomExecutorInnerContext>,
    > {
        self.inner_ctx.into_inner()
    }
}

use std::error::Error;
use std::fmt;

#[derive(Debug)]
enum CustomExecutorError {
    OutDirKeyMissing(String),
    OutDirValueMissing(String),
    InnerContextMutex(String),
    Io(io::Error, PathBuf),
}

impl Error for CustomExecutorError {}

impl fmt::Display for CustomExecutorError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl Executor for CustomExecutor {
    /// In case of an `Err`, Cargo will not continue with the build process for
    /// this package.
    fn exec(
        &self,
        cmd: ProcessBuilder,
        _id: &PackageId,
        _target: &Target,
    ) -> CargoResult<()> {
        let args = cmd.get_args();
        let out_dir_key = OsString::from("--out-dir");
        let out_dir_key_idx =
            args.iter().position(|s| *s == out_dir_key).ok_or_else(|| {
                CustomExecutorError::OutDirKeyMissing(cmd.to_string())
            })?;
        let out_dir = args
            .get(out_dir_key_idx + 1)
            .ok_or_else(|| {
                CustomExecutorError::OutDirValueMissing(cmd.to_string())
            }).map(PathBuf::from)?;

        // This can be different from the cwd used to launch the wrapping cargo
        // plugin. Discovered while fixing
        // https://github.com/anderejd/cargo-geiger/issues/19
        let cwd = cmd
            .get_cwd()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.cwd.to_owned());

        {
            // Scope to drop and release the mutex before calling rustc.
            let mut ctx = self.inner_ctx.lock().map_err(|e| {
                CustomExecutorError::InnerContextMutex(e.to_string())
            })?;
            for tuple in args
                .iter()
                .map(|s| (s, s.to_string_lossy().to_lowercase()))
                .filter(|t| t.1.ends_with(".rs"))
            {
                let raw_path = cwd.join(tuple.0);
                let p = raw_path
                    .canonicalize()
                    .map_err(|e| CustomExecutorError::Io(e, raw_path))?;
                ctx.rs_file_args.insert(p);
            }
            ctx.out_dir_args.insert(out_dir);
        }
        cmd.exec()?;
        Ok(())
    }

    /// TODO: Investigate if this returns the information we need through
    /// stdout or stderr.
    fn exec_json(
        &self,
        _cmd: ProcessBuilder,
        _id: &PackageId,
        _target: &Target,
        _handle_stdout: &mut FnMut(&str) -> CargoResult<()>,
        _handle_stderr: &mut FnMut(&str) -> CargoResult<()>,
    ) -> CargoResult<()> {
        //cmd.exec_with_streaming(handle_stdout, handle_stderr, false)?;
        //Ok(())
        unimplemented!();
    }

    /// Queried when queuing each unit of work. If it returns true, then the
    /// unit will always be rebuilt, independent of whether it needs to be.
    fn force_rebuild(&self, _unit: &Unit) -> bool {
        true // Overriding the default to force all units to be processed.
    }
}

/// TODO: Write proper documentation for this.
/// This function seems to be looking up the active flags for conditional
/// compilation (cargo::util::Cfg instances).
fn get_cfgs(
    config: &Config,
    target: &Option<String>,
    ws: &Workspace,
) -> CargoResult<Option<Vec<Cfg>>> {
    let mut process = util::process(&config.rustc(Some(ws))?.path);
    process.arg("--print=cfg").env_remove("RUST_LOG");
    if let Some(ref s) = *target {
        process.arg("--target").arg(s);
    }

    let output = match process.exec_with_output() {
        Ok(output) => output,
        Err(_) => return Ok(None),
    };
    let output = str::from_utf8(&output.stdout).unwrap();
    let lines = output.lines();
    Ok(Some(
        lines.map(Cfg::from_str).collect::<CargoResult<Vec<_>>>()?,
    ))
}

fn workspace(
    config: &Config,
    manifest_path: Option<PathBuf>,
) -> CargoResult<Workspace> {
    let root = match manifest_path {
        Some(path) => path,
        None => important_paths::find_root_manifest_for_wd(config.cwd())?,
    };
    Workspace::new(&root, config)
}

fn registry<'a>(
    config: &'a Config,
    package: &Package,
) -> CargoResult<PackageRegistry<'a>> {
    let mut registry = PackageRegistry::new(config)?;
    registry.add_sources(&[package.package_id().source_id().clone()])?;
    Ok(registry)
}

fn resolve<'a, 'cfg>(
    registry: &mut PackageRegistry<'cfg>,
    ws: &'a Workspace<'cfg>,
    features: Option<String>,
    all_features: bool,
    no_default_features: bool,
) -> CargoResult<(PackageSet<'a>, Resolve)> {
    let features =
        Method::split_features(&features.into_iter().collect::<Vec<_>>());

    let (packages, resolve) = ops::resolve_ws(ws)?;

    let method = Method::Required {
        dev_deps: true,
        features: &features,
        all_features,
        uses_default_features: !no_default_features,
    };

    let resolve = ops::resolve_with_previous(
        registry,
        ws,
        method,
        Some(&resolve),
        None,
        &[],
        true,
        true,
    )?;
    Ok((packages, resolve))
}

struct Node<'a> {
    id: &'a PackageId,
    pack: &'a Package,
}

struct Graph<'a> {
    graph: petgraph::Graph<Node<'a>, Kind>,
    nodes: HashMap<&'a PackageId, NodeIndex>,
}

/// Almost unmodified compared to the original in cargo-tree, should be fairly
/// simple to move this and the dependency graph structure out to a library.
/// TODO: Move this to a module to begin with.
fn build_graph<'a>(
    resolve: &'a Resolve,
    packages: &'a PackageSet,
    root: &'a PackageId,
    target: Option<&str>,
    cfgs: Option<&[Cfg]>,
) -> CargoResult<Graph<'a>> {
    let mut graph = Graph {
        graph: petgraph::Graph::new(),
        nodes: HashMap::new(),
    };
    let node = Node {
        id: root,
        pack: packages.get(root)?,
    };
    graph.nodes.insert(root, graph.graph.add_node(node));

    let mut pending = vec![root];

    while let Some(pkg_id) = pending.pop() {
        let idx = graph.nodes[&pkg_id];
        let pkg = packages.get(pkg_id)?;

        for raw_dep_id in resolve.deps_not_replaced(pkg_id) {
            let it = pkg
                .dependencies()
                .iter()
                .filter(|d| d.matches_id(raw_dep_id))
                .filter(|d| {
                    d.platform()
                        .and_then(|p| target.map(|t| p.matches(t, cfgs)))
                        .unwrap_or(true)
                });
            let dep_id = match resolve.replacement(raw_dep_id) {
                Some(id) => id,
                None => raw_dep_id,
            };
            for dep in it {
                let dep_idx = match graph.nodes.entry(dep_id) {
                    Entry::Occupied(e) => *e.get(),
                    Entry::Vacant(e) => {
                        pending.push(dep_id);
                        let node = Node {
                            id: dep_id,
                            pack: packages.get(dep_id)?,
                        };
                        *e.insert(graph.graph.add_node(node))
                    }
                };
                graph.graph.add_edge(idx, dep_idx, dep.kind());
            }
        }
    }

    Ok(graph)
}

fn print_tree<'a>(
    package: &'a PackageId,
    graph: &Graph<'a>,
    rs_files_used: &mut HashMap<PathBuf, u32>,
    pc: &PrintConfig,
) {
    let mut visited_deps = HashSet::new();
    let mut levels_continue = vec![];
    let node = &graph.graph[graph.nodes[&package]];
    print_dependency(
        node,
        &graph,
        &mut visited_deps,
        &mut levels_continue,
        rs_files_used,
        pc,
    );
}

fn print_dependency<'a>(
    package: &Node<'a>,
    graph: &Graph<'a>,
    visited_deps: &mut HashSet<&'a PackageId>,
    levels_continue: &mut Vec<bool>,
    rs_files_used: &mut HashMap<PathBuf, u32>,
    pc: &PrintConfig,
) {
    let new = pc.all || visited_deps.insert(package.id);
    let treevines = match pc.prefix {
        Prefix::Depth => format!("{} ", levels_continue.len()),
        Prefix::Indent => {
            let mut buf = String::new();
            if let Some((&last_continues, rest)) = levels_continue.split_last()
            {
                for &continues in rest {
                    let c = if continues { pc.symbols.down } else { " " };
                    buf.push_str(&format!("{}   ", c));
                }
                let c = if last_continues {
                    pc.symbols.tee
                } else {
                    pc.symbols.ell
                };
                buf.push_str(&format!("{0}{1}{1} ", c, pc.symbols.right));
            }
            buf
        }
        Prefix::None => "".into(),
    };

    // TODO: Find and collect unsafe stats as a separate pass over the deps
    // tree before printing.
    let counters = find_unsafe(
        package.pack.root(),
        rs_files_used,
        pc.allow_partial_results,
        pc.include_tests,
        pc.verbosity,
    );
    let unsafe_found = counters.has_unsafe();
    let colorize = |s: String| {
        if unsafe_found {
            s.red().bold()
        } else {
            s.green()
        }
    };
    let rad = if unsafe_found { "☢" } else { "" };
    let dep_name = colorize(format!(
        "{}",
        pc.format
            .display(package.id, package.pack.manifest().metadata())
    ));
    // TODO: Split up table and tree printing and paint into a backbuffer
    // before writing to stdout?
    let unsafe_info = colorize(table_row(&counters));
    println!("{}  {: <1} {}{}", unsafe_info, rad, treevines, dep_name);
    if !new {
        return;
    }
    let mut normal = vec![];
    let mut build = vec![];
    let mut development = vec![];
    for edge in graph
        .graph
        .edges_directed(graph.nodes[&package.id], pc.direction)
    {
        let dep = match pc.direction {
            EdgeDirection::Incoming => &graph.graph[edge.source()],
            EdgeDirection::Outgoing => &graph.graph[edge.target()],
        };
        match *edge.weight() {
            Kind::Normal => normal.push(dep),
            Kind::Build => build.push(dep),
            Kind::Development => development.push(dep),
        }
    }
    print_dependency_kind(
        Kind::Normal,
        normal,
        graph,
        visited_deps,
        levels_continue,
        rs_files_used,
        pc,
    );
    print_dependency_kind(
        Kind::Build,
        build,
        graph,
        visited_deps,
        levels_continue,
        rs_files_used,
        pc,
    );
    print_dependency_kind(
        Kind::Development,
        development,
        graph,
        visited_deps,
        levels_continue,
        rs_files_used,
        pc,
    );
}

fn print_dependency_kind<'a>(
    kind: Kind,
    mut deps: Vec<&Node<'a>>,
    graph: &Graph<'a>,
    visited_deps: &mut HashSet<&'a PackageId>,
    levels_continue: &mut Vec<bool>,
    rs_files_used: &mut HashMap<PathBuf, u32>,
    pc: &PrintConfig,
) {
    if deps.is_empty() {
        return;
    }

    // Resolve uses Hash data types internally but we want consistent output ordering
    deps.sort_by_key(|n| n.id);

    let name = match kind {
        Kind::Normal => None,
        Kind::Build => Some("[build-dependencies]"),
        Kind::Development => Some("[dev-dependencies]"),
    };
    if let Prefix::Indent = pc.prefix {
        if let Some(name) = name {
            print!("{}", table_row_empty());
            for &continues in &**levels_continue {
                let c = if continues { pc.symbols.down } else { " " };
                print!("{}   ", c);
            }

            println!("{}", name);
        }
    }

    let mut it = deps.iter().peekable();
    while let Some(dependency) = it.next() {
        levels_continue.push(it.peek().is_some());
        print_dependency(
            dependency,
            graph,
            visited_deps,
            levels_continue,
            rs_files_used,
            pc,
        );
        levels_continue.pop();
    }
}

// TODO: use a table library, or factor the tableness out in a smarter way
const UNSAFE_COUNTERS_HEADER: [&str; 6] = [
    "Functions ",
    "Expressions ",
    "Impls ",
    "Traits ",
    "Methods ",
    "Dependency",
];

fn table_row_empty() -> String {
    " ".repeat(
        UNSAFE_COUNTERS_HEADER
            .iter()
            .take(5)
            .map(|s| s.len())
            .sum::<usize>()
            + UNSAFE_COUNTERS_HEADER.len()
            + 1,
    )
}

fn table_row(cb: &CounterBlock) -> String {
    let calc_total = |c: &Count| c.unsafe_used + c.unsafe_unused;
    let fmt = |c: &Count| format!("{}/{}", c.unsafe_used, calc_total(c));
    format!(
        "{: <10} {: <12} {: <6} {: <7} {: <7}",
        fmt(&cb.functions),
        fmt(&cb.exprs),
        fmt(&cb.itemimpls),
        fmt(&cb.itemtraits),
        fmt(&cb.methods),
    )
}
