//! Runnable evidence for the `unwrap`-free design question.
//!
//! Three techniques, in increasing order of cost:
//!
//! 1. **Required arguments in the constructor.** Free, and it solves most
//!    real builders. `build()` is infallible because there is nothing to
//!    check.
//! 2. **Typestate.** Tracks which fields are set in the *type*, so `build()`
//!    exists only once every required field is present. Costs one type
//!    parameter per required field.
//! 3. **Refinement / smart constructors.** Validate once at the boundary,
//!    then carry a type that cannot hold an invalid value. This is what
//!    turns "parse, don't validate" into a compiler-checked property.
//!
//! Run with: `cargo run --example typestate`

// Demonstration types: fields are read via `Debug`, setters exist to show the
// shape of the API rather than because this example calls all of them.
#![allow(dead_code)]

use std::fmt;
use std::marker::PhantomData;

// =====================================================================
// 1. Required arguments in the constructor
// =====================================================================

/// The boring solution, and usually the right one.
///
/// `name` and `port` are required, so they are parameters of `new`. The
/// optional fields have defaults. `build` cannot fail, so it returns `T`
/// and there is no `unwrap` for a caller to write.
#[derive(Debug)]
struct SimpleConfig {
    name: String,
    port: u16,
    retries: u8,
    verbose: bool,
}

struct SimpleBuilder {
    name: String,
    port: u16,
    retries: u8,
    verbose: bool,
}

impl SimpleBuilder {
    fn new(name: impl Into<String>, port: u16) -> Self {
        SimpleBuilder {
            name: name.into(),
            port,
            retries: 3,
            verbose: false,
        }
    }
    fn retries(mut self, n: u8) -> Self {
        self.retries = n;
        self
    }
    fn verbose(mut self, v: bool) -> Self {
        self.verbose = v;
        self
    }
    /// Note the return type: `SimpleConfig`, not `Result<SimpleConfig, _>`.
    fn build(self) -> SimpleConfig {
        SimpleConfig {
            name: self.name,
            port: self.port,
            retries: self.retries,
            verbose: self.verbose,
        }
    }
}

// =====================================================================
// 2. Typestate
// =====================================================================
//
// Use this when required fields cannot go in the constructor — because
// there are many of them, because they arrive in an arbitrary order, or
// because the builder is passed around and filled in by different code.

/// Marker: this field has been supplied.
#[derive(Debug)]
struct Set;
/// Marker: this field has not been supplied.
#[derive(Debug)]
struct Unset;

#[derive(Debug)]
struct Connection {
    host: String,
    port: u16,
    timeout_ms: u32,
}

/// `HOST` and `PORT` are type-level booleans tracking which required fields
/// have been set.
struct ConnBuilder<HOST, PORT> {
    host: Option<String>,
    port: Option<u16>,
    timeout_ms: u32,
    _marker: PhantomData<(HOST, PORT)>,
}

impl ConnBuilder<Unset, Unset> {
    fn new() -> Self {
        ConnBuilder {
            host: None,
            port: None,
            timeout_ms: 5_000,
            _marker: PhantomData,
        }
    }
}

impl<HOST, PORT> ConnBuilder<HOST, PORT> {
    /// Setting the host flips the first type parameter to `Set`.
    fn host(self, host: impl Into<String>) -> ConnBuilder<Set, PORT> {
        ConnBuilder {
            host: Some(host.into()),
            port: self.port,
            timeout_ms: self.timeout_ms,
            _marker: PhantomData,
        }
    }

    fn port(self, port: u16) -> ConnBuilder<HOST, Set> {
        ConnBuilder {
            host: self.host,
            port: Some(port),
            timeout_ms: self.timeout_ms,
            _marker: PhantomData,
        }
    }

    /// Optional — does not change the type.
    fn timeout_ms(mut self, ms: u32) -> Self {
        self.timeout_ms = ms;
        self
    }
}

/// `build` exists **only** for a fully configured builder. Calling it too
/// early is a "method not found" compile error, not a runtime panic.
impl ConnBuilder<Set, Set> {
    fn build(self) -> Connection {
        // The `Option`s are statically known to be `Some`, but the compiler
        // cannot see that through `PhantomData`. `unwrap_or_default` keeps
        // the function total without weakening the external guarantee.
        //
        // The alternative that removes even this is technique 2b below.
        Connection {
            host: self.host.unwrap_or_default(),
            port: self.port.unwrap_or_default(),
            timeout_ms: self.timeout_ms,
        }
    }
}

// --- 2b. Typestate that carries the data, so there are no Options at all --

/// Instead of `PhantomData` markers plus `Option` fields, let the type
/// parameter *be* the storage: `()` when unset, `T` when set. Now `build`
/// destructures real values and no `unwrap` is reachable even internally.
struct Conn2<H, P> {
    host: H,
    port: P,
    timeout_ms: u32,
}

impl Conn2<(), ()> {
    fn new() -> Self {
        Conn2 {
            host: (),
            port: (),
            timeout_ms: 5_000,
        }
    }
}

impl<H, P> Conn2<H, P> {
    fn host(self, host: impl Into<String>) -> Conn2<String, P> {
        Conn2 {
            host: host.into(),
            port: self.port,
            timeout_ms: self.timeout_ms,
        }
    }
    fn port(self, port: u16) -> Conn2<H, u16> {
        Conn2 {
            host: self.host,
            port,
            timeout_ms: self.timeout_ms,
        }
    }
    fn timeout(mut self, ms: u32) -> Self {
        self.timeout_ms = ms;
        self
    }
}

impl Conn2<String, u16> {
    /// Total, and with no internal `unwrap` either.
    fn build(self) -> Connection {
        Connection {
            host: self.host,
            port: self.port,
            timeout_ms: self.timeout_ms,
        }
    }
}

// =====================================================================
// 3. Refinement types — validate once, then it is a compiler fact
// =====================================================================

/// A `u16` that is known to be non-zero *and* outside the privileged range.
///
/// The only way to obtain one is [`Port::new`], which returns `Option`. After
/// that, every function taking a `Port` is total.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Port(u16);

impl Port {
    fn new(v: u16) -> Option<Self> {
        (v >= 1024).then_some(Port(v))
    }
    fn get(self) -> u16 {
        self.0
    }
}

/// The signature does the documenting: this cannot be handed a bad port, so
/// it cannot fail, so there is nothing to unwrap.
fn bind(port: Port) -> String {
    format!("bound to 0.0.0.0:{}", port.get())
}

// =====================================================================
// Where types genuinely cannot help
// =====================================================================

#[derive(Debug)]
enum AppError {
    BadPort(u16),
    Json(String),
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AppError::BadPort(p) => write!(f, "port {p} is reserved"),
            AppError::Json(e) => write!(f, "json: {e}"),
        }
    }
}

impl std::error::Error for AppError {}

/// External input is *inherently* fallible. The right answer is not to make
/// it infallible, it is to convert it into a domain type once, at the edge,
/// with `?` — and then never check it again.
fn load(port_text: &str, doc: &[u8]) -> Result<String, AppError> {
    let raw: u16 = port_text.parse().map_err(|_| AppError::BadPort(0))?;
    let port = Port::new(raw).ok_or(AppError::BadPort(raw))?;

    let mut parser = dacode_json::strict::StrictParser::new();
    let parsed = parser
        .parse(doc)
        .map_err(|e| AppError::Json(e.to_string()))?;

    let name = parsed
        .root()
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or(std::borrow::Cow::Borrowed("<anonymous>"));

    // `bind` takes a `Port`, so from here on the validity of the port is a
    // fact the compiler enforces rather than a comment.
    Ok(format!("{name}: {}", bind(port)))
}

/// `?` inside a closure returns from the *closure*, so the closure has to
/// return `Result` and the caller has to consume it. `collect` into a
/// `Result` is the idiom that makes this ergonomic.
fn parse_all(inputs: &[&str]) -> Result<Vec<Port>, AppError> {
    inputs
        .iter()
        .map(|s| {
            let raw: u16 = s.parse().map_err(|_| AppError::BadPort(0))?;
            Port::new(raw).ok_or(AppError::BadPort(raw))
        })
        .collect()
}

// =====================================================================

/// `main` returning `Result` is what lets `?` replace `unwrap` at the top
/// level. The same applies to `#[test] fn t() -> Result<(), E>`.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = SimpleBuilder::new("edge", 8080).retries(5).build();
    println!("1.  constructor args : {cfg:?}");

    let conn = ConnBuilder::new().port(9000).host("db.internal").build();
    println!("2.  typestate        : {conn:?}");

    let conn2 = Conn2::new()
        .host("db.internal")
        .port(9000)
        .timeout(250)
        .build();
    println!("2b. typestate + data : {conn2:?}");

    // This is the payoff. Uncomment either line and the crate stops
    // compiling with "no method named `build` found":
    //
    //     let _ = ConnBuilder::new().host("only-host").build();
    //     let _ = Conn2::new().port(9000).build();
    //
    // Not a runtime panic. Not a `Result` the caller might `unwrap`. A
    // compile error.

    println!(
        "3.  refinement       : {}",
        bind(Port::new(8080).ok_or("bad port")?)
    );
    println!("    rejected         : {:?}", Port::new(80));

    println!(
        "4.  edge conversion  : {}",
        load("8080", br#"{"name":"svc"}"#)?
    );
    println!(
        "    error path       : {:?}",
        load("80", br#"{"name":"svc"}"#)
    );
    println!("    error path       : {:?}", load("8080", b"{oops").err());

    println!("5.  closures + ?     : {:?}", parse_all(&["1024", "8080"])?);
    println!("    error path       : {:?}", parse_all(&["1024", "80"]));

    Ok(())
}
