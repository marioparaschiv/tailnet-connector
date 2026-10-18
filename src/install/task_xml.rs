//! The definition of the connector's scheduled task, in the Task Scheduler 1.2 schema.

/// Starts `command` hidden at `user`'s logon with `user`'s environment, and restarts it a minute
/// after it fails.
pub(super) fn task_xml(command: &str, working_dir: &str, user: &str) -> String {
    let command = escape(command);
    let working_dir = escape(working_dir);
    let user = escape(user);

    format!(
        r#"<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <StartWhenAvailable>true</StartWhenAvailable>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>999</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
      <WorkingDirectory>{working_dir}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#
    )
}

fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());

    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(character),
        }
    }

    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_the_command_at_the_users_logon_and_restarts_it() {
        let xml = task_xml(
            r"C:\Users\ada\bin\tailnet-connector.exe",
            r"C:\Users\ada\bin",
            r"DESKTOP\ada",
        );

        assert_eq!(
            xml,
            r#"<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>DESKTOP\ada</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>DESKTOP\ada</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <StartWhenAvailable>true</StartWhenAvailable>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>999</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>C:\Users\ada\bin\tailnet-connector.exe</Command>
      <WorkingDirectory>C:\Users\ada\bin</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#
        );
    }

    #[test]
    fn escapes_the_paths_and_the_user() {
        let xml = task_xml(
            r"C:\Tom & Jerry's <tools>\tailnet-connector.exe",
            r#"C:\Tom & Jerry's <tools>"#,
            r#"DESKTOP\"ada""#,
        );

        assert!(xml.contains(
            r"<Command>C:\Tom &amp; Jerry&apos;s &lt;tools&gt;\tailnet-connector.exe</Command>"
        ));
        assert!(xml.contains(
            r"<WorkingDirectory>C:\Tom &amp; Jerry&apos;s &lt;tools&gt;</WorkingDirectory>"
        ));
        assert_eq!(
            xml.matches(r"<UserId>DESKTOP\&quot;ada&quot;</UserId>")
                .count(),
            2
        );
    }
}
