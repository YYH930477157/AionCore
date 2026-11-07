//! Rules that tell an agent where to keep the files it creates.
//!
//! The client picks a per-conversation work folder and output folder inside
//! the user's workspace (`extra.session_files`). Every agent family receives
//! the same text: aionrs in its system prompt, ACP and Antigravity in the
//! preset context of their first message.

use aionui_api_types::SessionFiles;

/// Said only when a folder path has non-ASCII characters (the folder names follow the UI
/// language). In a live run MinGW `ld` could not open `-o` with such a full path even
/// though the system code page covered the characters; relative paths from inside the
/// folder worked.
const NON_ASCII_PATH_HINT: &str = "These paths contain non-ASCII characters, and some command-line tools, such as compilers and linkers, cannot open them. \
     When a tool reads or writes files in these folders, run it with the folder as its working directory and give it relative paths \
     (for example `-o test.exe`) rather than full paths.";

/// The rules block for one conversation's folders.
pub(crate) fn session_files_rules(files: &SessionFiles) -> String {
    let work = files.work_dir.trim();
    let output = files.output_dir.trim();
    let mut text = format!(
        "## Files you create\n\
         \n\
         This conversation has its own folders for the files you create. Use them so the project stays clean:\n\
         \n\
         - Work folder: `{work}`. Put scripts, scratch code, test data, logs and other intermediate or temporary files here.\n\
         - Output folder: `{output}`. Put finished deliverables the user asked for here, such as reports, exported data, charts or generated documents, unless the user names another place.\n\
         \n\
         Changes to the project itself (its source code, tests, configuration and documentation) are made in place in the project, not in these folders. \
         Create a folder when you first write a file into it. When you tell the user about a file in these folders, give its full path."
    );
    if !work.is_ascii() || !output.is_ascii() {
        text.push_str("\n\n");
        text.push_str(NON_ASCII_PATH_HINT);
    }
    text
}

/// Append the rules block to existing rules, keeping `None` when there is
/// nothing to add and nothing to start from.
pub(crate) fn append_session_files_rules(base: Option<String>, files: Option<&SessionFiles>) -> Option<String> {
    let base = base.filter(|text| !text.trim().is_empty());
    match (base, files) {
        (base, None) => base,
        (None, Some(files)) => Some(session_files_rules(files)),
        (Some(base), Some(files)) => Some(format!("{base}\n\n{}", session_files_rules(files))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files() -> SessionFiles {
        SessionFiles {
            work_dir: "C:\\repo\\.woody\\会话文件\\2026-11-06_修复串口\\过程".to_owned(),
            output_dir: "C:\\repo\\.woody\\会话文件\\2026-11-06_修复串口\\成品".to_owned(),
        }
    }

    #[test]
    fn rules_name_both_folders_and_keep_project_edits_in_place() {
        let text = session_files_rules(&files());

        assert!(text.contains("Work folder: `C:\\repo\\.woody\\会话文件\\2026-11-06_修复串口\\过程`"));
        assert!(text.contains("Output folder: `C:\\repo\\.woody\\会话文件\\2026-11-06_修复串口\\成品`"));
        assert!(text.contains("made in place in the project, not in these folders"));
        assert!(text.contains("unless the user names another place"));
    }

    #[test]
    fn non_ascii_folders_get_the_relative_path_hint() {
        let text = session_files_rules(&files());

        assert!(text.ends_with(NON_ASCII_PATH_HINT));
        assert!(text.contains("give its full path.\n\nThese paths contain non-ASCII characters"));
    }

    #[test]
    fn ascii_folders_get_no_path_hint() {
        let text = session_files_rules(&SessionFiles {
            work_dir: "C:\\repo\\.woody\\chat-files\\2026-11-06_fix_uart\\work".to_owned(),
            output_dir: "C:\\repo\\.woody\\chat-files\\2026-11-06_fix_uart\\output".to_owned(),
        });

        assert!(!text.contains("non-ASCII"));
        assert!(text.ends_with("give its full path."));
    }

    #[test]
    fn rules_follow_existing_rules_after_a_blank_line() {
        let text = append_session_files_rules(Some("Always answer in Chinese.".to_owned()), Some(&files())).unwrap();

        assert!(text.starts_with("Always answer in Chinese.\n\n## Files you create\n"));
    }

    #[test]
    fn rules_stand_alone_when_there_are_no_other_rules() {
        for base in [None, Some(String::new()), Some("  \n".to_owned())] {
            let text = append_session_files_rules(base, Some(&files())).unwrap();
            assert!(text.starts_with("## Files you create\n"));
        }
    }

    #[test]
    fn nothing_changes_without_session_files() {
        assert_eq!(
            append_session_files_rules(Some("rules".to_owned()), None).as_deref(),
            Some("rules")
        );
        assert_eq!(append_session_files_rules(None, None), None);
    }
}
