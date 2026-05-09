use std::io::{BufRead, Write};

use crate::model::PaymentAttempt;
use crate::policy::PolicyDecision;

pub trait ApprovalUi {
    fn confirm(&mut self, attempt: &PaymentAttempt, decision: &PolicyDecision) -> bool;
}

pub struct CliApproval<R: BufRead, W: Write> {
    reader: R,
    writer: W,
}

impl<R: BufRead, W: Write> CliApproval<R, W> {
    pub fn new(reader: R, writer: W) -> Self {
        Self { reader, writer }
    }
}

impl<R: BufRead, W: Write> ApprovalUi for CliApproval<R, W> {
    fn confirm(&mut self, attempt: &PaymentAttempt, decision: &PolicyDecision) -> bool {
        let reason = decision.reason().unwrap_or("approval required");
        writeln!(self.writer, "─── Approval Required ───").ok();
        writeln!(self.writer, "Reason:    {reason}").ok();
        writeln!(self.writer, "Agent:     {}", attempt.agent_id).ok();
        writeln!(self.writer, "Task:      {}", attempt.task_id).ok();
        writeln!(self.writer, "Merchant:  {}", attempt.merchant).ok();
        writeln!(
            self.writer,
            "Amount:    {} {}",
            attempt.amount, attempt.currency
        )
        .ok();
        if let Some(n) = &attempt.network {
            writeln!(self.writer, "Network:   {n}").ok();
        }
        loop {
            write!(self.writer, "Approve? [y/N]: ").ok();
            self.writer.flush().ok();
            let mut line = String::new();
            if self.reader.read_line(&mut line).unwrap_or(0) == 0 {
                return false;
            }
            match line.trim().to_lowercase().as_str() {
                "y" | "yes" => return true,
                "n" | "no" | "" => return false,
                _ => {
                    writeln!(self.writer, "Please answer 'y' or 'n'.").ok();
                }
            }
        }
    }
}

pub fn stdin_approval() -> CliApproval<std::io::BufReader<std::io::Stdin>, std::io::Stdout> {
    CliApproval::new(std::io::BufReader::new(std::io::stdin()), std::io::stdout())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{PaymentAttempt, Protocol};
    use rust_decimal::Decimal;
    use std::io::Cursor;
    use std::str::FromStr;

    fn fake_attempt() -> PaymentAttempt {
        PaymentAttempt::new(
            Protocol::X402,
            "agent",
            "task",
            "api.example.com",
            Decimal::from_str("0.25").unwrap(),
            "USDC",
        )
    }

    #[test]
    fn approves_on_yes() {
        let input = Cursor::new(b"y\n".to_vec());
        let mut output = Vec::new();
        let mut ui = CliApproval::new(input, &mut output);
        let decision = PolicyDecision::RequireApproval {
            reason: "above threshold".to_string(),
        };
        assert!(ui.confirm(&fake_attempt(), &decision));
    }

    #[test]
    fn rejects_on_no() {
        let input = Cursor::new(b"no\n".to_vec());
        let mut output = Vec::new();
        let mut ui = CliApproval::new(input, &mut output);
        let decision = PolicyDecision::RequireApproval {
            reason: "above threshold".to_string(),
        };
        assert!(!ui.confirm(&fake_attempt(), &decision));
    }

    #[test]
    fn re_prompts_on_garbage() {
        let input = Cursor::new(b"maybe\nyes\n".to_vec());
        let mut output = Vec::new();
        let mut ui = CliApproval::new(input, &mut output);
        let decision = PolicyDecision::RequireApproval {
            reason: "above threshold".to_string(),
        };
        assert!(ui.confirm(&fake_attempt(), &decision));
    }
}
