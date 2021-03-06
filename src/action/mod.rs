//! Covers all user triggered actions (except for signals).

use super::*;
use anyhow::{anyhow, Result};
use log::{debug, trace};

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};

use std::path::PathBuf;

pub mod bandaid;
pub mod interactive;

pub(crate) use bandaid::*;

/// State of conclusion.
#[derive(Debug, Clone, Copy)]
pub enum Finish {
    /// Abort is user requested, either by signal or key stroke.
    Abort,
    /// Completion of the check run, with the resulting number of
    /// mistakes accumulated.
    MistakeCount(usize),
}

impl Finish {
    /// A helper to determine if any mistakes were found.
    pub fn found_any(&self) -> bool {
        match *self {
            Self::MistakeCount(n) if n > 0 => true,
            _ => false,
        }
    }
}

/// A patch to be stitched ontop of another string.
///
/// Has intentionally no awareness of any rust or cmark/markdown semantics.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) enum Patch {
    /// Replace the area spanned by `replace` with `replacement`.
    /// Since `Span` is inclusive, `Replace` always will replace a character in the original sources.
    Replace {
        replace_span: Span,
        replacement: String,
    },
    /// Location where to insert.
    Insert {
        insert_at: LineColumn,
        content: String,
    },
}

impl<'a> From<&'a BandAid> for Patch {
    fn from(bandaid: &'a BandAid) -> Self {
        // TODO XXX
        Self::from(bandaid.clone())
    }
}

impl From<BandAid> for Patch {
    fn from(bandaid: BandAid) -> Self {
        match bandaid {
            bandaid if bandaid.span.start == bandaid.span.end => Self::Insert {
                insert_at: bandaid.span.start,
                content: bandaid.content,
            },
            _ => Self::Replace {
                replace_span: bandaid.span,
                replacement: bandaid.content,
            },
        }
    }
}

/// Correct lines by applying patches.
///
/// Assumes all `BandAids` do not overlap when replacing.
/// Inserting multiple times at a particular `LineColumn` is ok,
/// but replacing overlapping `Span`s of the original source is not.
///
/// This function is not concerend with _any_ semantics or comments or
/// whatsoever at all, it blindly replaces what is given to it.
pub(crate) fn apply_patches<'s, II, I>(
    patches: II,
    source_buffer: &str,
    mut sink: impl Write,
) -> Result<()>
where
    II: IntoIterator<IntoIter = I, Item = Patch>,
    I: Iterator<Item = Patch>,
{
    let mut patches = patches.into_iter().peekable();

    let mut source_iter =
        iter_with_line_column_from(source_buffer, LineColumn { line: 1, column: 0 }).peekable();

    const TARGET: &str = "patch";
    let mut write_to_sink = |topic: &str, data: &str| -> Result<()> {
        log::trace!(target: TARGET, "w<{}>: {}", topic, data.escape_debug());
        sink.write(data.as_bytes())?;
        Ok(())
    };

    let mut cc_end_byte_offset = 0;

    let mut current = None;
    let mut byte_cursor = 0usize;
    loop {
        let cc_start_byte_offset = if let Some(ref current) = current {
            let (cc_start, data, insertion) = match current {
                Patch::Replace {
                    replace_span,
                    replacement,
                } => (replace_span.end, replacement.as_str(), false),
                Patch::Insert { insert_at, content } => (insert_at.clone(), content.as_str(), true),
            };

            write_to_sink("new", data)?;

            if insertion {
                // do not advance anythin on insertion
                byte_cursor
            } else {
                // skip the range of chars based on the line column
                // so the cursor continues after the "replaced" characters
                let mut cc_start_byte_offset = byte_cursor;
                'skip: while let Some((c, byte_offset, _idx, linecol)) = source_iter.peek() {
                    let byte_offset = *byte_offset;
                    let linecol = *linecol;

                    cc_start_byte_offset = byte_offset + c.len_utf8();

                    if linecol >= cc_start {
                        log::trace!(
                            target: TARGET,
                            "skip buffer: >{}<",
                            &source_buffer[cc_end_byte_offset..cc_start_byte_offset].escape_debug()
                        );

                        break 'skip;
                    }

                    log::trace!(target: TARGET, "skip[{}]: >{}<", _idx, c.escape_debug());

                    let _ = source_iter.next();
                }
                cc_start_byte_offset
            }
        } else {
            byte_cursor
        };
        debug_assert!(byte_cursor <= cc_start_byte_offset);
        byte_cursor = cc_start_byte_offset;

        cc_end_byte_offset = if let Some(upcoming) = patches.peek() {
            let cc_end = match upcoming {
                Patch::Replace { replace_span, .. } => replace_span.start,
                Patch::Insert { insert_at, .. } => insert_at.clone(),
            };

            // do not write anything

            // carbon copy until this byte offset
            let mut cc_end_byte_offset = byte_cursor;
            'cc: while let Some((c, byte_offset, _idx, linecol)) = source_iter.peek() {
                let byte_offset = *byte_offset;
                let linecol = *linecol;

                if linecol >= cc_end {
                    log::trace!(
                        target: TARGET,
                        "copy buffer: >{}<",
                        &source_buffer[cc_start_byte_offset..cc_end_byte_offset].escape_debug()
                    );
                    break 'cc;
                }

                cc_end_byte_offset = byte_offset + c.len_utf8();

                log::trace!(target: TARGET, "copy[{}]: >{}<", _idx, c.escape_debug());

                let _ = source_iter.next();
                // we need to drag this one behind, since...
            }
            // in the case we reach EOF here the `cc_end_byte_offset` could never be updated correctly
            std::cmp::min(cc_end_byte_offset, source_buffer.len())
        } else {
            source_buffer.len()
        };
        debug_assert!(byte_cursor <= cc_end_byte_offset);

        byte_cursor = cc_end_byte_offset;

        let cc_range = cc_start_byte_offset..cc_end_byte_offset;

        write_to_sink("cc", &source_buffer[cc_range])?;

        // move on to the next
        current = patches.next();

        if current.is_none() {
            // we already made sure earlier to write out everything
            break;
        }
    }

    Ok(())
}

/// Mode in which `cargo-spellcheck` operates
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Action {
    /// Only show errors
    Check,
    /// Interactively choose from checker provided suggestions.
    Fix,
    /// Reflow doc comments so they adhere to a given maximum column width.
    Reflow,
}

impl Action {
    /// Apply bandaids to the file represented by content origin.
    pub fn write_changes_to_disk(
        &self,
        origin: ContentOrigin,
        bandaids: impl IntoIterator<Item = BandAid>,
    ) -> Result<()> {
        match origin {
            ContentOrigin::CommonMarkFile(path) => self.correct_file(path, bandaids),
            ContentOrigin::RustSourceFile(path) => self.correct_file(path, bandaids),
            ContentOrigin::RustDocTest(path, _span) => self.correct_file(path, bandaids),
            #[cfg(test)]
            ContentOrigin::TestEntityRust => unreachable!("Use a proper file"),
            #[cfg(test)]
            ContentOrigin::TestEntityCommonMark => unreachable!("Use a proper file"),
        }
    }

    /// assumes suggestions are sorted by line number and column number and must be non overlapping
    fn correct_file(
        &self,
        path: PathBuf,
        bandaids: impl IntoIterator<Item = BandAid>,
    ) -> Result<()> {
        let path = path
            .as_path()
            .canonicalize()
            .map_err(|e| anyhow!("Failed to canonicalize {}", path.display()).context(e))?;
        let path = path.as_path();
        trace!("Attempting to open {} as read", path.display());
        let ro = std::fs::OpenOptions::new()
            .read(true)
            .open(path)
            .map_err(|e| anyhow!("Failed to open {}", path.display()).context(e))?;

        let mut reader = std::io::BufReader::new(ro);

        const TEMPORARY: &'static str = ".spellcheck.tmp";

        let tmp = std::env::current_dir()
            .expect("Must have cwd")
            .join(TEMPORARY);
        // let tmp = tmp.canonicalize().map_err(|e| { anyhow!("Failed to canonicalize {}", tmp.display() ).context(e) })?;
        //trace!("Attempting to open {} as read", tmp.display());
        let wr = OpenOptions::new()
            .write(true)
            .truncate(true)
            .create(true)
            .open(&tmp)
            .map_err(|e| anyhow!("Failed to open {}", path.display()).context(e))?;

        let mut writer = std::io::BufWriter::with_capacity(1024, wr);

        let mut content = String::with_capacity(2e6 as usize);
        reader.get_mut().read_to_string(&mut content)?;

        apply_patches(
            bandaids.into_iter().map(|x| Patch::from(x)),
            content.as_str(), // FIXME for efficiency, correct_lines should integrate with `BufRead` instead of a `String` buffer
            &mut writer,
        )?;

        writer.flush()?;

        fs::rename(tmp, path)?;

        Ok(())
    }

    /// Consumingly apply the user picked changes to a file.
    ///
    /// **Attention**: Must be consuming, repeated usage causes shifts in spans and
    /// would destroy the file structure!
    pub fn write_user_pick_changes_to_disk(
        &self,
        userpicked: interactive::UserPicked,
        _config: &Config,
    ) -> Result<()> {
        if userpicked.total_count() > 0 {
            debug!("Writing changes back to disk");
            for (origin, bandaids) in userpicked.bandaids.into_iter() {
                self.write_changes_to_disk(origin, bandaids.into_iter())?;
            }
        } else {
            debug!("No band aids to apply");
        }
        Ok(())
    }

    /// Purpose was to check, checking complete, so print the results.
    fn check(&self, suggestions_per_path: SuggestionSet, _config: &Config) -> Result<Finish> {
        let mut count = 0usize;
        for (_path, suggestions) in suggestions_per_path {
            count += suggestions.len();
            for suggestion in suggestions {
                println!("{}", suggestion);
            }
        }
        Ok(Finish::MistakeCount(count))
    }

    /// Run the requested action.
    pub fn run(self, suggestions: SuggestionSet, config: &Config) -> Result<Finish> {
        match self {
            Self::Check => self.check(suggestions, config),
            Self::Reflow => {
                let n = suggestions.len();

                for (origin, suggestions) in suggestions {
                    let bandaids = suggestions
                        .into_iter()
                        .filter_map(|suggestion| {
                            suggestion.replacements.first().map(|replacement| {
                                let bandaid = super::BandAid::from((
                                    replacement.to_owned(),
                                    &suggestion.span,
                                ));
                                bandaid
                            })
                        })
                        .collect::<Vec<_>>();
                    self.write_changes_to_disk(origin, bandaids)?;
                }
                Ok(Finish::MistakeCount(n))
            }
            Self::Fix => {
                let (picked, user_sel) =
                    interactive::UserPicked::select_interactive(suggestions, config)?;
                if user_sel == interactive::UserSelection::Abort {
                    Ok(Finish::Abort)
                } else {
                    let n = picked.total_count();
                    self.write_user_pick_changes_to_disk(picked, config)?;
                    Ok(Finish::MistakeCount(n))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::TryInto;

    macro_rules! verify_correction {
        ($text:literal, $bandaids:expr, $expected:literal) => {
            let mut sink: Vec<u8> = Vec::with_capacity(1024);

            apply_patches(
                $bandaids.into_iter().map(|bandaid| Patch::from(bandaid)),
                $text,
                &mut sink,
            )
            .expect("Line correction must work in unit test!");

            assert_eq!(String::from_utf8_lossy(sink.as_slice()), $expected);
        };
    }

    #[test]
    fn patch_full() {
        let _ = env_logger::Builder::new()
            .filter_level(log::LevelFilter::Trace)
            .is_test(true)
            .try_init();

        let patches = vec![
            Patch::Replace {
                replace_span: Span {
                    start: LineColumn { line: 1, column: 6 },
                    end: LineColumn {
                        line: 2,
                        column: 12,
                    },
                },
                replacement: "& Omega".to_owned(),
            },
            Patch::Insert {
                insert_at: LineColumn { line: 3, column: 0 },
                content: "Icecream truck".to_owned(),
            },
        ];
        verify_correction!(
            r#"Alpha beta gamma
zeta eta beta.
"#,
            patches,
            r#"Alpha & Omega.
Icecream truck"#
        );
    }

    #[test]
    fn patch_replace_1() {
        let _ = env_logger::Builder::new()
            .filter_level(log::LevelFilter::Trace)
            .is_test(true)
            .try_init();
        let bandaids = vec![Patch::Replace {
            replace_span: (1_usize, 0..1).try_into().unwrap(),
            replacement: "Y".to_owned(),
        }];
        verify_correction!("T🐠🐠U", bandaids, "Y🐠🐠U");
    }

    #[test]
    fn patch_replace_2() {
        let _ = env_logger::Builder::new()
            .filter_level(log::LevelFilter::Trace)
            .is_test(true)
            .try_init();
        let bandaids = vec![Patch::Replace {
            replace_span: (1_usize, 1..3).try_into().unwrap(),
            replacement: "Y".to_owned(),
        }];
        verify_correction!("T🐠🐠U", bandaids, "TYU");
    }

    #[test]
    fn patch_replace_3() {
        let _ = env_logger::Builder::new()
            .filter_level(log::LevelFilter::Trace)
            .is_test(true)
            .try_init();
        let bandaids = vec![Patch::Replace {
            replace_span: (1_usize, 3..4).try_into().unwrap(),
            replacement: "Y".to_owned(),
        }];
        verify_correction!("T🐠🐠U", bandaids, "T🐠🐠Y");
    }

    #[test]
    fn patch_injection_1() {
        let _ = env_logger::Builder::new()
            .filter_level(log::LevelFilter::Trace)
            .is_test(true)
            .try_init();

        let patches = vec![Patch::Insert {
            insert_at: LineColumn {
                line: 1_usize,
                column: 0,
            },
            content: "Q".to_owned(),
        }];
        verify_correction!("A🐢C", patches, "QA🐢C");
    }

    #[test]
    fn patch_injection_2() {
        let _ = env_logger::Builder::new()
            .filter_level(log::LevelFilter::Trace)
            .is_test(true)
            .try_init();

        let patches = vec![Patch::Insert {
            insert_at: LineColumn {
                line: 1_usize,
                column: 2,
            },
            content: "Q".to_owned(),
        }];
        verify_correction!("A🐢C", patches, "A🐢QC");
    }
    #[test]
    fn patch_injection_3() {
        let _ = env_logger::Builder::new()
            .filter_level(log::LevelFilter::Trace)
            .is_test(true)
            .try_init();

        let patches = vec![Patch::Insert {
            insert_at: LineColumn {
                line: 1_usize,
                column: 3,
            },
            content: "Q".to_owned(),
        }];
        verify_correction!("A🐢C", patches, "A🐢CQ");
    }
}
