// SPDX-License-Identifier: GPL-2.0-only
//! The agent: model, tool call, tool result, model, until `done`.
//!
//! The program is one loop over a copy of the conversation. Every step builds
//! a SmolLM2 chat prompt, encodes it with [`encode::Encoder`], asks the
//! runtime for a tool call constrained by [`madrust_infer::grammar`], parses
//! the call, runs the tool, and appends the assistant turn and the result to
//! the history. [`tools::NAMES`] is the whole registry.
//!
//! The default task, used when the guest passes no argument, reads
//! `/vocab.json` and reports how many bytes it has. The loop stops at the
//! `done` tool or after [`MAX_STEPS`] steps.
//!
//! The output is one line per event, so the rig can read a run without a
//! parser for prose:
//!
//! ```text
//! AGENT step=<n> tool=<name> args=<json> result=<text>
//! AGENT answer=<text>
//! AGENT steps=<n> tokens=<n> seconds=<f64> tool_calls=<n>
//! ```

#![deny(unsafe_op_in_unsafe_fn)]
// A host build that is not the guest has no server to drive; the modules and
// the helpers exist for the tests in that build.
#![cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]

mod call;
mod encode;
mod expression;
mod tools;

/// Steps the loop may take before it gives up.
pub const MAX_STEPS: usize = 4;

/// Tokens one step may generate.
///
/// A tool call is a short JSON object, but the grammar allows merged tokens
/// and the model can choose many one byte tokens before it closes the call.
pub const MAX_NEW_TOKENS: usize = 256;

/// The task used when the program gets no argument.
pub const DEFAULT_TASK: &str =
    "Read the file /vocab.json and report how many bytes it has. Use the read_file tool, then call done with the byte count.";

/// The system text: the protocol and the tool list.
pub const SYSTEM: &str = "You are a tool using agent. Answer with exactly one JSON tool call and nothing else, in the form {\"tool\":\"NAME\",\"args\":{...}}. The arguments are read_file(path), list_dir(path), write_file(path,text), compute(value) and done(text). Call done with the final answer when the task is complete.";

/// The task from the command line, or the default when there is none.
pub fn default_task(arguments: &[String]) -> String {
    let task = arguments.join(" ");
    let task = task.trim();
    if task.is_empty() {
        DEFAULT_TASK.to_string()
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

/// The summary line of a run.
pub fn summary(steps: usize, tokens: usize, seconds: f64, tool_calls: usize) -> String {
    format!("AGENT steps={steps} tokens={tokens} seconds={seconds:.3} tool_calls={tool_calls}")
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

/// Drive the loop until `done` or [`MAX_STEPS`].
#[cfg(target_os = "linux")]
fn run(task: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut server = Server::open()?;
    let encoder = encode::Encoder::new(server.vocab())?;
    let grammar = Grammar::tool_call(&tools::NAMES);
    let mut history: Vec<(Role, String)> = Vec::new();

    let started = Instant::now();
    let mut tokens = 0usize;
    let mut steps = 0usize;
    let mut answer: Option<String> = None;

    for step in 1..=MAX_STEPS {
        let prompt = chat::tool_prompt(SYSTEM, task, &tools::NAMES, &history);
        let ids = encoder.encode(&prompt)?;
        let generation = server.generate(&ids, MAX_NEW_TOKENS, Some(&grammar))?;
        tokens = tokens.saturating_add(generation.tokens.len());

        let written = String::from_utf8_lossy(&generation.bytes).into_owned();
        let call = match call::Call::parse(&generation.bytes) {
            Ok(call) => call,
            Err(error) => {
                eprintln!("AGENT ERR step={step} {error}");
                break;
            }
        };

        steps += 1;
        let outcome = tools::run(&call);
        let result = match outcome {
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

        history.push((Role::Assistant, written));
        history.push((Role::User, result));

        if answer.is_some() {
            break;
        }
    }

    if let Some(answer) = answer {
        println!("{}", answer_line(&answer));
    }
    println!(
        "{}",
        summary(steps, tokens, started.elapsed().as_secs_f64(), steps)
    );
    Ok(())
}

/// Ask the machine to go down, so Linux does not panic when PID 1 exits.
///
/// Mainline Linux panics with "Attempted to kill init!" when the last process
/// leaves, and the agent is PID 1 there. A kernel that does not know the call
/// answers with an error and the process leaves the ordinary way, which is what
/// MadRust does. The benchmark workload and the inference runtime use the same
/// approach.
#[cfg(target_os = "linux")]
fn power_off() {
    #[cfg(target_arch = "x86_64")]
    {
        const SYS_REBOOT: u64 = 169;
        const LINUX_REBOOT_MAGIC1: u64 = 0x0123_4567;
        const LINUX_REBOOT_MAGIC2: u64 = 0xCDEF_0123;
        const LINUX_REBOOT_CMD_POWER_OFF: u64 = 0x4321_FEDC;

        // SAFETY: reboot takes three arguments. On a kernel that accepts it the
        // call never returns; on a kernel that refuses it, the result is an
        // error number and the caller continues.
        unsafe {
            core::arch::asm!(
                "syscall",
                inlateout("rax") SYS_REBOOT as i64 => _,
                in("rdi") LINUX_REBOOT_MAGIC1,
                in("rsi") LINUX_REBOOT_CMD_POWER_OFF,
                in("rdx") LINUX_REBOOT_MAGIC2,
                lateout("rcx") _,
                lateout("r11") _,
                options(nostack),
            );
        }
    }
}

/// Flush the log, ask the machine to go down, and leave with `status`.
#[cfg(target_os = "linux")]
fn leave(status: i32) -> ! {
    use std::io::Write;
    let _ = std::io::stdout().flush();
    power_off();
    std::process::exit(status)
}

#[cfg(target_os = "linux")]
fn main() {
    let arguments: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect();
    let task = default_task(&arguments);
    if let Err(error) = run(&task) {
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
    use super::{answer_line, default_task, sanitize, step_line, summary, DEFAULT_TASK};

    #[test]
    fn no_argument_uses_the_default_task() {
        assert_eq!(default_task(&[]), DEFAULT_TASK);
        assert_eq!(default_task(&["  ".to_string()]), DEFAULT_TASK);
        assert_eq!(
            default_task(&["read".to_string(), "/a".to_string()]),
            "read /a"
        );
    }

    #[test]
    fn a_newline_is_written_as_an_escape() {
        assert_eq!(sanitize("a\nb\rc\td"), "a\\nb\\rc\\td");
    }

    #[test]
    fn the_output_lines_have_the_rig_shape() {
        assert_eq!(
            step_line(1, "read_file", r#"{"path":"/a"}"#, "bytes=1\ncontent=a"),
            r#"AGENT step=1 tool=read_file args={"path":"/a"} result=bytes=1\ncontent=a"#
        );
        assert_eq!(answer_line("done\nnow"), "AGENT answer=done\\nnow");
        assert_eq!(
            summary(2, 17, 1.5, 2),
            "AGENT steps=2 tokens=17 seconds=1.500 tool_calls=2"
        );
    }
}
