use crate::{emit, session::Event};
use std::io;

#[test]
fn lifecycle_diagnostics_cover_every_event_and_propagate_write_failure() {
    let mut output = Vec::new();
    emit(
        Event::Mounted {
            target: "/srv/alpha".into(),
            sources: vec!["/srv/source".into()],
        },
        &mut output,
    )
    .unwrap();
    emit(
        Event::Unmounted {
            target: "/srv/alpha".into(),
        },
        &mut output,
    )
    .unwrap();
    emit(
        Event::Skipped {
            target: "/srv/one".into(),
            reason: "overlap".into(),
        },
        &mut output,
    )
    .unwrap();
    emit(
        Event::Warning(workspace_overlay::Error::Invalid("watch failed".into())),
        &mut output,
    )
    .unwrap();
    let text = String::from_utf8(output).unwrap();
    assert!(text.contains("Mounted /srv/alpha + /srv/source; Ctrl-C to unmount.\n"));
    assert!(text.contains("Unmounted /srv/alpha\n"));
    assert!(text.contains("Skipping worktree /srv/one: overlap\n"));
    assert!(text.contains("WARN: watch failed\n"));
    struct Broken;
    impl io::Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::from_raw_os_error(libc::ENOSPC))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    assert!(
        emit(
            Event::Unmounted {
                target: "/srv/alpha".into()
            },
            &mut Broken
        )
        .is_err()
    );
}
