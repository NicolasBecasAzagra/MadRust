// SPDX-License-Identifier: GPL-2.0-only
//! The agent: model, tool call, tool result, model, until `done`.
//!
//! The program is one loop over a copy of the conversation. Every step builds
//! a SmolLM2 chat prompt, encodes it with [`encode::Encoder`], asks the
//! runtime for a tool call constrained by [`madrust_infer::grammar`], parses
//! the call, runs the tool, and appends the assistant turn and the result to
//! the history. [`tools::NAMES`] is the whole registry.
//!
//! The default task, used when the guest passes no argument, sends one
//! datagram to the server of the host and reports the answer: the tool of the
//! network is what closes the goal of the project. The loop stops at the `done`
//! tool or after [`MAX_STEPS`] steps.
//!
//! Neither kernel hands arguments to the program it starts as init, so the
//! address and the port of the server reach the agent through `/net.conf` of
//! the initramfs, the file the network client of the rig reads too:
//!
//! ```text
//! address=10.0.2.2
//! port=43512
//! selftest=1
//! ```
//!
//! `selftest=1` makes the agent call the network tool once before it asks the
//! model, with the same code path the model uses, and print the result as an
//! `AGENT selftest` line. The gate of the pipeline uses it: the model of that
//! run is the synthetic one, which decides nothing sensible, so a gate that
//! waited for it to choose the network tool would test the model and not the
//! kernel.
//!
//! The grammar bounds the strings of the call: an argument string holds at
//! most `madrust_infer::grammar::DEFAULT_MAX_STRING_LENGTH` characters (128 by
//! default), and one escape sequence counts as one character. The tool names
//! and the argument keys are literals and are not bounded. A tool call carries
//! a path, a command or a short answer, so the bound costs no useful call.
//! It is what keeps a call finite: without it a model that opens a string and
//! never closes it samples until [`MAX_NEW_TOKENS`] runs out and writes no
//! call at all. A caller that needs another bound calls
//! `Grammar::tool_call_with_limit` instead of `Grammar::tool_call`.
//!
//! [`tools::REQUIREMENTS`] is part of the grammar too: a call of a tool is
//! accepted only when its argument object carries the keys that tool needs,
//! under values of the type each key declares, so the model cannot answer
//! `{"tool":"read_file","args":{}}` or `{"tool":"read_file","args":{"path":false}}`
//! and spend a step on a call the tool would refuse.
//!
//! The output is one line per event, so the rig can read a run without a
//! parser for prose:
//!
//! ```text
//! AGENT step=<n> tool=<name> args=<json> result=<text>
//! AGENT selftest tool=net_send args=<json> result=<text>
//! AGENT answer=<text>
//! AGENT steps=<n> tokens=<n> seconds=<f64> tool_calls=<n> [net_us=<n> retries=<n>]
//! AGENT DONE
//! ```

#![deny(unsafe_op_in_unsafe_fn)]
// A host build that is not the guest has no server to drive; the modules and
// the helpers exist for the tests in that build.
#![cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]

mod call;
mod encode;
mod expression;
mod tools;

use std::net::Ipv4Addr;

/// Steps the loop may take before it gives up.
pub const MAX_STEPS: usize = 4;

/// Characters of the conversation the prompt keeps.
///
/// Every turn of the conversation is encoded and prefilled again on the next
/// step, so a history without a bound makes every step cost more than the
/// last one. The oldest turns are dropped when the budget is exceeded.
#[cfg(target_os = "linux")]
const MAX_HISTORY_CHARS: usize = 4096;

/// Tokens one step may generate.
///
/// A tool call is a short JSON object, but the grammar allows merged tokens
/// and the model can choose many one byte tokens before it closes the call.
/// The grammar bounds every argument string, so the budget is not the only
/// thing that ends an unterminated call.
///
/// The budget is what a call needs with room to spare: a name, a key and a
/// path of up to a hundred and twenty eight characters.
pub const MAX_NEW_TOKENS: usize = 96;

/// The task used when the program gets no argument.
///
/// `{endpoint}` is replaced with the address and the port of the server, read
/// from `/net.conf`, so the model calls the tool with the endpoint of this run.
pub const DEFAULT_TASK: &str = "Send the datagram \"ping\" to the server at {endpoint} with the net_send tool, then call done with the answer of the server.";

/// The system text: the protocol and the tool list.
pub const SYSTEM: &str = "You are a tool using agent. Answer with exactly one JSON tool call and nothing else, in the form {\"tool\":\"NAME\",\"args\":{...}}. The arguments are read_file(path), list_dir(path), write_file(path,text), compute(value), net_send(query,text) and done(text). net_send sends the text as one datagram of UDP to the server named by query as ADDRESS:PORT and returns its answer. Call done with the final answer when the task is complete.";

/// Path of the file that carries the endpoint of the server.
pub const CONFIG_PATH: &str = "/net.conf";

/// Address of the server when the initramfs does not name one.
pub const DEFAULT_ADDRESS: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);

/// Port of the server when the initramfs does not name one.
pub const DEFAULT_PORT: u16 = 9999;

/// Where the server of the run listens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Endpoint {
    /// Address of the server.
    pub address: Ipv4Addr,
    /// Port of the server.
    pub port: u16,
    /// Whether the agent sends its own datagram before it asks the model.
    pub selftest: bool,
}

impl Default for Endpoint {
    fn default() -> Self {
        Self {
            address: DEFAULT_ADDRESS,
            port: DEFAULT_PORT,
            selftest: false,
        }
    }
}

impl Endpoint {
    /// Read the endpoint from `/net.conf`, or use the defaults.
    ///
    /// A missing file is not an error: a run by hand on a normal Linux may have
    /// no initramfs to put one in, and the default names the host of the
    /// emulator.
    pub fn load() -> Result<Self, String> {
        match std::fs::read_to_string(CONFIG_PATH) {
            Ok(text) => Self::from_text(&text),
            Err(_) => Ok(Self::default()),
        }
    }

    /// Parse the `key=value` lines of the configuration file.
    pub fn from_text(text: &str) -> Result<Self, String> {
        let mut endpoint = Self::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(format!("config: line without a key: {line}"));
            };
            let value = value.trim();
            match key.trim() {
                "address" => {
                    endpoint.address = value
                        .parse()
                        .map_err(|_| format!("config: address: {value}"))?
                }
                "port" => {
                    endpoint.port = value
                        .parse()
                        .map_err(|_| format!("config: port: {value}"))?
                }
                "selftest" => endpoint.selftest = parse_flag(value)?,
                other => return Err(format!("config: unknown key: {other}")),
            }
        }
        if endpoint.port == 0 {
            return Err("config: port must be positive".to_string());
        }
        Ok(endpoint)
    }

    /// The endpoint in the `ADDRESS:PORT` form the tool takes.
    pub fn query(&self) -> String {
        format!("{}:{}", self.address, self.port)
    }
}

/// Parse a boolean of the configuration file.
fn parse_flag(value: &str) -> Result<bool, String> {
    match value {
        "1" | "true" | "yes" => Ok(true),
        "0" | "false" | "no" => Ok(false),
        other => Err(format!("config: selftest: {other}")),
    }
}

/// The task from the command line, or the default when there is none.
pub fn default_task(arguments: &[String], endpoint: &str) -> String {
    let task = arguments.join(" ");
    let task = task.trim();
    if task.is_empty() {
        DEFAULT_TASK.replace("{endpoint}", endpoint)
    } else {
        task.to_string()
    }
}

/// Put a result on one line: a newline inside it would split the `AGENT` line.
pub fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            character => out.push(character),
        }
    }
    out
}

/// One step line.
pub fn step_line(step: usize, tool: &str, args: &str, result: &str) -> String {
    format!(
        "AGENT step={step} tool={tool} args={args} result={}",
        sanitize(result)
    )
}

/// The final answer line.
pub fn answer_line(text: &str) -> String {
    format!("AGENT answer={}", sanitize(text))
}

/// Characters of the conversation the prompt would carry.
#[cfg(target_os = "linux")]
fn history_chars(history: &[(madrust_infer::chat::Role, String)]) -> usize {
    history.iter().map(|(_, text)| text.len()).sum()
}

/// Whether a call is the same one the model already made.
///
/// A model that asks for the same thing twice is not going to ask for
/// something else, and the loop would spend its whole budget on the same
/// call. The run stops there and says so.
pub fn repeats(previous: Option<&str>, request: &str) -> bool {
    previous == Some(request)
}

/// The summary line of a run.
///
/// The round trip of a network call is on the same line as the cost of the
/// run, because it happens inside the run: the rig publishes it as a row of its
/// own and the seconds of the agent already carry it.
pub fn summary(
    steps: usize,
    tokens: usize,
    seconds: f64,
    tool_calls: usize,
    net: Option<tools::RoundTrip>,
) -> String {
    let mut line =
        format!("AGENT steps={steps} tokens={tokens} seconds={seconds:.3} tool_calls={tool_calls}");
    if let Some(net) = net {
        line.push_str(&format!(" net_us={} retries={}", net.micros, net.retries));
    }
    line
}

#[cfg(target_os = "linux")]
use std::time::Instant;

#[cfg(target_os = "linux")]
use madrust_infer::chat::{self, Role};
#[cfg(target_os = "linux")]
use madrust_infer::grammar::Grammar;
#[cfg(target_os = "linux")]
use madrust_infer::runtime::Server;

#[cfg(target_os = "linux")]
use crate::tools::Outcome;

/// Send one datagram with the network tool, outside the model.
///
/// This is the deterministic proof the gate uses: the same [`tools::run`] the
/// loop calls, the same grammar arguments, and a line of the transcript with
/// the tool name and the answer. It runs before the model is asked, so a model
/// whose answers are arbitrary cannot hide a broken tool. The configuration
/// file decides whether it runs.
#[cfg(target_os = "linux")]
fn selftest(endpoint: &Endpoint) -> Option<tools::RoundTrip> {
    use std::collections::BTreeMap;

    /// Text the agent sends on its own, so the answer from the server names
    /// this call and not a datagram of the model.
    const SELFTEST_TEXT: &str = "ping-agent";

    let mut args = BTreeMap::new();
    args.insert(
        "query".to_string(),
        crate::call::Value::String(endpoint.query()),
    );
    args.insert(
        "text".to_string(),
        crate::call::Value::String(SELFTEST_TEXT.to_string()),
    );
    let call = crate::call::Call {
        tool: "net_send".to_string(),
        args,
    };
    let step = tools::run(&call);
    let text = match &step.outcome {
        Outcome::Continue(text) | Outcome::Done(text) => text.clone(),
    };
    println!(
        "AGENT selftest tool=net_send args={} result={}",
        call.args_json(),
        sanitize(&text)
    );
    step.round_trip
}

/// Drive the loop until `done` or [`MAX_STEPS`].
#[cfg(target_os = "linux")]
fn run(task: &str, endpoint: &Endpoint) -> Result<(), Box<dyn std::error::Error>> {
    let mut server = Server::open()?;
    let encoder = encode::Encoder::new(server.vocab())?;
    // The default bound on the argument strings: a path, a command or the
    // answer of `done` fits, and an unterminated string stops the call. The
    // grammar carries the keys of each tool too, with the type of value each
    // key takes, so it never generates a call the tool cannot read.
    let grammar = Grammar::tool_call_with_requirements(&tools::NAMES, tools::REQUIREMENTS)?;
    let mut history: Vec<(Role, String)> = Vec::new();

    let started = Instant::now();
    let mut tokens = 0usize;
    let mut steps = 0usize;
    let mut answer: Option<String> = None;
    let mut previous: Option<String> = None;
    // The clock of the run starts before the selftest, so the round trip of the
    // network is inside the seconds of the agent and not beside them.
    let mut net = if endpoint.selftest {
        selftest(endpoint)
    } else {
        None
    };

    for step in 1..=MAX_STEPS {
        let prompt = chat::tool_prompt(SYSTEM, task, &tools::NAMES, &history);
        let ids = encoder.encode(server.vocab(), &prompt)?;
        let generation = server.generate(&ids, MAX_NEW_TOKENS, Some(&grammar))?;
        tokens = tokens.saturating_add(generation.tokens.len());

        let written = String::from_utf8_lossy(&generation.bytes).into_owned();
        let call = match call::Call::parse(&generation.bytes) {
            Ok(call) => call,
            Err(error) => {
                // What the model wrote is in the log too: a call that does not
                // parse is the answer of the model, and the answer is what
                // says why it did not parse.
                eprintln!("AGENT ERR step={step} {error}");
                println!(
                    "{}",
                    step_line(
                        step,
                        "?",
                        "{}",
                        &sanitize(&String::from_utf8_lossy(&generation.bytes)),
                    )
                );
                break;
            }
        };

        steps += 1;
        // A model that asks for the same thing twice is not going to ask for
        // something else: the loop would spend its whole budget on the same
        // call. The run stops there and says so, instead of burning the time
        // of the reader.
        let request = format!("{} {}", call.tool, call.args_json());
        if repeats(previous.as_deref(), &request) {
            println!(
                "{}",
                step_line(
                    step,
                    &call.tool,
                    &call.args_json(),
                    "the model repeated the call"
                )
            );
            break;
        }
        previous = Some(request);

        let run = tools::run(&call);
        if run.round_trip.is_some() {
            net = run.round_trip;
        }
        let result = match run.outcome {
            Outcome::Continue(result) => result,
            Outcome::Done(result) => {
                answer = Some(result.clone());
                result
            }
        };
        println!(
            "{}",
            step_line(step, &call.tool, &call.args_json(), &result)
        );

        // The history is the prompt of the next step, and the prompt is
        // encoded and prefilled again every time. A history without a bound
        // makes every step cost more than the last one, so the turns that do
        // not fit in the budget are dropped from the oldest end.
        history.push((Role::Assistant, written));
        history.push((Role::User, result));
        while history.len() > 2 && history_chars(&history) > MAX_HISTORY_CHARS {
            history.remove(0);
        }

        if answer.is_some() {
            break;
        }
    }

    if let Some(answer) = answer {
        println!("{}", answer_line(&answer));
    }
    println!(
        "{}",
        summary(steps, tokens, started.elapsed().as_secs_f64(), steps, net)
    );
    // The rig stops the guest on this marker, so it does not wait for the
    // machine to leave and does not depend on the exit status of the program.
    println!("AGENT DONE");
    Ok(())
}

/// Flush the log, ask the machine to go down, and leave with `status`.
#[cfg(target_os = "linux")]
fn leave(status: i32) -> ! {
    use std::io::Write;
    let _ = std::io::stdout().flush();
    madrust_infer::power_off::power_off();
    std::process::exit(status)
}

#[cfg(target_os = "linux")]
fn main() {
    let arguments: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect();
    let endpoint = match Endpoint::load() {
        Ok(endpoint) => endpoint,
        Err(error) => {
            eprintln!("AGENT ERR {error}");
            leave(1);
        }
    };
    let task = default_task(&arguments, &endpoint.query());
    if let Err(error) = run(&task, &endpoint) {
        eprintln!("AGENT ERR {error}");
        leave(1);
    }
    leave(0);
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("madrust-agent: build the agent for x86_64-unknown-linux-musl");
    std::process::exit(2);
}

#[cfg(test)]
mod tests {
    use super::{
        answer_line, default_task, repeats, sanitize, step_line, summary, Endpoint,
        DEFAULT_ADDRESS, DEFAULT_PORT, DEFAULT_TASK,
    };
    use crate::tools::RoundTrip;

    #[test]
    fn no_argument_uses_the_default_task_with_the_endpoint() {
        let endpoint = "10.0.2.2:43512";
        assert_eq!(
            default_task(&[], endpoint),
            DEFAULT_TASK.replace("{endpoint}", endpoint)
        );
        assert_eq!(
            default_task(&["  ".to_string()], endpoint),
            DEFAULT_TASK.replace("{endpoint}", endpoint)
        );
        assert!(default_task(&[], endpoint).contains(endpoint));
        assert_eq!(
            default_task(&["read".to_string(), "/a".to_string()], endpoint),
            "read /a"
        );
    }

    #[test]
    fn the_file_carries_the_endpoint_and_the_selftest() {
        let endpoint = Endpoint::from_text(
            "# written by the rig\naddress = 10.0.2.2\nport=43512\nselftest=1\n",
        )
        .expect("the file is valid");
        assert_eq!(endpoint.address, DEFAULT_ADDRESS);
        assert_eq!(endpoint.port, 43512);
        assert!(endpoint.selftest);
        assert_eq!(endpoint.query(), "10.0.2.2:43512");
        assert_eq!(Endpoint::from_text(""), Ok(Endpoint::default()));
        assert_eq!(Endpoint::default().port, DEFAULT_PORT);
        assert!(!Endpoint::default().selftest);
    }

    #[test]
    fn a_bad_endpoint_is_an_error_and_not_a_default() {
        assert!(Endpoint::from_text("port=99999\n").is_err());
        assert!(Endpoint::from_text("speed=1\n").is_err());
        assert!(Endpoint::from_text("address\n").is_err());
        assert!(Endpoint::from_text("port=0\n").is_err());
        assert!(Endpoint::from_text("selftest=perhaps\n").is_err());
    }

    #[test]
    fn a_newline_is_written_as_an_escape() {
        assert_eq!(sanitize("a\nb\rc\td"), "a\\nb\\rc\\td");
    }

    #[test]
    fn a_call_that_repeats_is_recognized() {
        assert!(repeats(
            Some("read_file {\"path\":\"/a\"}"),
            "read_file {\"path\":\"/a\"}"
        ));
        assert!(!repeats(
            Some("read_file {\"path\":\"/a\"}"),
            "read_file {\"path\":\"/b\"}"
        ));
        assert!(!repeats(None, "read_file {\"path\":\"/a\"}"));
    }

    #[test]
    fn the_output_lines_have_the_rig_shape() {
        assert_eq!(
            step_line(1, "read_file", r#"{"path":"/a"}"#, "bytes=1\ncontent=a"),
            r#"AGENT step=1 tool=read_file args={"path":"/a"} result=bytes=1\ncontent=a"#
        );
        assert_eq!(answer_line("done\nnow"), "AGENT answer=done\\nnow");
        assert_eq!(
            summary(2, 17, 1.5, 2, None),
            "AGENT steps=2 tokens=17 seconds=1.500 tool_calls=2"
        );
        assert_eq!(
            summary(
                2,
                17,
                1.5,
                2,
                Some(RoundTrip {
                    micros: 420,
                    retries: 3
                })
            ),
            "AGENT steps=2 tokens=17 seconds=1.500 tool_calls=2 net_us=420 retries=3"
        );
    }
}
