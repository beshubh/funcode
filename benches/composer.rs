use criterion::{
    BatchSize, BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main,
};
use funcode::{
    agent::AgentEvent,
    app::{App, Screen},
    composer::{ComposerDocument, SubmittedContent},
    session::SessionMode,
    submission::{SubmissionEvent, SubmissionTaskRunner},
    theme::{Theme, ThemeId},
    transcript::ToolArtifact,
    ui::{self, UiRenderer},
    workspace::{Attachment, WorkspacePath},
};
use ratatui::{Terminal, backend::TestBackend};
use std::{path::PathBuf, thread, time::Duration};

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;
const WIDTH: usize = 80;
const VIEWPORT_HEIGHT: usize = 24;

fn document_with_single_line(size: usize) -> ComposerDocument {
    let mut document = ComposerDocument::default();
    document.insert_text(&"x".repeat(size));
    document
}

fn document_with_multiline_paste(raw: &str) -> ComposerDocument {
    let mut document = ComposerDocument::default();
    let proposal = document.propose_paste(raw).unwrap();
    document.commit_paste(proposal).unwrap();
    document
}

fn cold_layout_size_matrix(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("composer/cold layout size matrix");
    for size in [KIB, 16 * KIB, 256 * KIB, MIB] {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(size),
            &size,
            |benchmark, &size| {
                benchmark.iter_batched(
                    || document_with_single_line(size),
                    |document| black_box(document.layout(WIDTH).total_rows()),
                    BatchSize::LargeInput,
                );
            },
        );
    }
    group.finish();
}

fn paste_benchmarks(criterion: &mut Criterion) {
    let one_mib = "x".repeat(MIB);
    let five_mib = "x".repeat(5 * MIB);

    criterion.bench_function("composer/reject 5 MiB paste", |benchmark| {
        let document = ComposerDocument::default();
        let original_revision = document.revision();
        benchmark.iter(|| {
            let rejected = document.propose_paste(black_box(&five_mib)).is_err();
            assert_eq!(document.revision(), original_revision);
            black_box(rejected)
        });
    });

    criterion.bench_function("composer/accept 1 MiB single-line paste", |benchmark| {
        benchmark.iter_batched(
            ComposerDocument::default,
            |mut document| {
                let proposal = document.propose_paste(black_box(&one_mib)).unwrap();
                black_box(document.commit_paste(proposal).unwrap());
            },
            BatchSize::LargeInput,
        );
    });

    criterion.bench_function(
        "composer/cold layout accepted 1 MiB single line",
        |benchmark| {
            benchmark.iter_batched(
                || {
                    let mut document = ComposerDocument::default();
                    let proposal = document.propose_paste(&one_mib).unwrap();
                    document.commit_paste(proposal).unwrap();
                    document
                },
                |document| black_box(document.layout(WIDTH).total_rows()),
                BatchSize::LargeInput,
            );
        },
    );

    let multiline = "line\n".repeat(16_000);
    criterion.bench_function("composer/multiline paste block insertion", |benchmark| {
        benchmark.iter_batched(
            ComposerDocument::default,
            |mut document| {
                let proposal = document.propose_paste(black_box(&multiline)).unwrap();
                black_box(document.commit_paste(proposal).unwrap());
            },
            BatchSize::LargeInput,
        );
    });

    criterion.bench_function(
        "composer/multiline paste block layout and render",
        |benchmark| {
            benchmark.iter_batched(
                || document_with_multiline_paste(&multiline).freeze(),
                |content| {
                    let layout = content.layout(WIDTH);
                    black_box(layout.visible_rows(0, VIEWPORT_HEIGHT));
                },
                BatchSize::LargeInput,
            );
        },
    );
}

fn scale_and_edit_benchmarks(criterion: &mut Criterion) {
    let seventy_thousand_rows = format!("{}tail", "x\n".repeat(70_000));
    criterion.bench_function("composer/seventy thousand hard rows", |benchmark| {
        benchmark.iter_batched(
            || {
                let mut document = ComposerDocument::default();
                document.insert_text(&seventy_thousand_rows);
                document
            },
            |document| black_box(document.layout(WIDTH).total_rows()),
            BatchSize::LargeInput,
        );
    });

    let mut group = criterion.benchmark_group("composer/1 MiB edits");
    for position in ["front", "middle", "end"] {
        group.bench_function(BenchmarkId::from_parameter(position), |benchmark| {
            benchmark.iter_batched(
                || {
                    let mut document = if position == "middle" {
                        let mut document = ComposerDocument::default();
                        document.insert_text(&format!(
                            "{}\n{}",
                            "x".repeat(MIB / 2),
                            "x".repeat(MIB / 2 - 1)
                        ));
                        document.move_home();
                        document
                    } else {
                        let mut document = document_with_single_line(MIB);
                        if position == "front" {
                            document.move_home();
                        }
                        document
                    };
                    // Keep setup semantically observable and outside the timed edit.
                    black_box(&mut document);
                    document
                },
                |mut document| {
                    document.insert_text("Z");
                    document.backspace();
                    black_box(document);
                },
                BatchSize::LargeInput,
            );
        });
    }
    group.finish();
}

fn cached_layout_and_navigation_benchmarks(criterion: &mut Criterion) {
    let mut document = document_with_single_line(MIB);
    let layout = document.layout(WIDTH);

    criterion.bench_function("composer/cached visible rows", |benchmark| {
        benchmark.iter(|| {
            black_box(layout.visible_rows(
                layout.total_rows().saturating_sub(VIEWPORT_HEIGHT),
                VIEWPORT_HEIGHT,
            ));
        });
    });

    // The paired move returns to the same cursor, so every iteration performs
    // valid navigation against the already-built 80-column layout.
    criterion.bench_function("composer/cached valid vertical navigation", |benchmark| {
        benchmark.iter(|| {
            document.move_up(WIDTH);
            document.move_down(WIDTH);
            let cached = document.layout(WIDTH);
            black_box(document.cursor_geometry(&cached));
        });
    });

    criterion.bench_function("composer/cold resize layout 79/80/120", |benchmark| {
        benchmark.iter_batched(
            || document.clone(),
            |resized| {
                black_box(resized.layout(79));
                black_box(resized.layout(80));
                black_box(resized.layout(120));
            },
            BatchSize::LargeInput,
        );
    });
}

fn submission_preflight_benchmark(criterion: &mut Criterion) {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("benchmark-context.txt"),
        "a".repeat(128 * KIB),
    )
    .unwrap();
    let root = workspace
        .path()
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("."));
    let runner = SubmissionTaskRunner::spawn(root);
    let attachment =
        Attachment::workspace_file(WorkspacePath::from_raw("benchmark-context.txt".to_owned()));
    let content = SubmittedContent::with_attachments("x".repeat(128 * KIB), &[attachment]);
    let mut draft_id = 0u64;

    criterion.bench_function(
        "submission/background preflight 128 KiB prompt and attachment",
        |benchmark| {
            benchmark.iter(|| {
                draft_id = draft_id.wrapping_add(1);
                runner
                    .request(draft_id, content.clone(), SessionMode::Build)
                    .unwrap();
                loop {
                    if let Some(event) = runner.try_event() {
                        match event {
                            SubmissionEvent::Prepared {
                                draft_id: prepared_id,
                                request,
                            } => {
                                assert_eq!(prepared_id, draft_id);
                                assert_eq!(request.attachments().len(), 1);
                                black_box(request.serialized_bytes());
                                break;
                            }
                            SubmissionEvent::Failed { message, .. } => {
                                panic!("submission benchmark preflight failed: {message}")
                            }
                            SubmissionEvent::Cancelled { .. } => {
                                panic!("submission benchmark preflight was cancelled")
                            }
                        }
                    }
                    thread::yield_now();
                }
            });
        },
    );
}

fn transcript_rendering_benchmarks(criterion: &mut Criterion) {
    let theme = Theme::resolve(ThemeId::Terminal);

    let mut large_app = App::new();
    large_app.screen = Screen::Chat;
    large_app
        .transcript
        .submit_content(1, SubmittedContent::plain("x".repeat(MIB)));
    let mut large_terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    // Prime the immutable content's layout cache. This benchmark exercises the
    // normal virtualized redraw path, not cold wrapping (covered above).
    large_terminal
        .draw(|frame| {
            black_box(ui::render(frame, &large_app, &theme));
        })
        .unwrap();
    criterion.bench_function("transcript/cached render 1 MiB user message", |benchmark| {
        benchmark.iter(|| {
            large_terminal
                .draw(|frame| {
                    black_box(ui::render(frame, &large_app, &theme));
                })
                .unwrap();
        });
    });

    let multiline = "line\n".repeat(16_000);
    let content = document_with_multiline_paste(&multiline).freeze();
    let mut paste_app = App::new();
    paste_app.screen = Screen::Chat;
    paste_app.transcript.submit_content(1, content);
    let mut paste_terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    criterion.bench_function("transcript/render multiline paste summary", |benchmark| {
        benchmark.iter(|| {
            paste_terminal
                .draw(|frame| {
                    black_box(ui::render(frame, &paste_app, &theme));
                })
                .unwrap();
        });
    });
}

fn transcript_stress_app() -> App {
    let mut app = App::new();
    app.screen = Screen::Chat;
    for request_id in 1..=200 {
        app.transcript
            .submit(request_id, format!("prompt {request_id}"), Vec::new());
        app.handle_agent_event(AgentEvent::Started { request_id });
        app.handle_agent_event(AgentEvent::ToolStarted {
            request_id,
            call_id: request_id,
            name: "terminal".into(),
            summary: format!("command {request_id}"),
            artifacts: vec![ToolArtifact::Terminal {
                description: format!("command {request_id}"),
                command: "printf output".into(),
                output: format!("output {request_id}\n"),
                exit_code: Some(0),
            }],
        });
        app.handle_agent_event(AgentEvent::TextDelta {
            request_id,
            text: format!("answer {request_id}"),
        });
        app.handle_agent_event(AgentEvent::Completed { request_id });
    }
    let request_id = 201;
    app.transcript
        .submit(request_id, "large artifact".into(), Vec::new());
    app.handle_agent_event(AgentEvent::Started { request_id });
    app.handle_agent_event(AgentEvent::ToolStarted {
        request_id,
        call_id: request_id,
        name: "terminal".into(),
        summary: "1,000-line output".into(),
        artifacts: vec![ToolArtifact::Terminal {
            description: "1,000-line output".into(),
            command: "generate-output".into(),
            output: (0..1_000)
                .map(|line| format!("artifact-line-{line}"))
                .collect::<Vec<_>>()
                .join("\n"),
            exit_code: Some(0),
        }],
    });
    app
}

fn transcript_interaction_benchmarks(criterion: &mut Criterion) {
    let theme = Theme::resolve(ThemeId::Terminal);
    let mut app = transcript_stress_app();
    let renderer = UiRenderer::default();
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    let mut maximum = 0;
    terminal
        .draw(|frame| {
            maximum = renderer
                .render(frame, &app, &theme)
                .transcript_scroll_maximum();
        })
        .unwrap();
    app.update_transcript_scroll_maximum(maximum);

    criterion.bench_function(
        "transcript/cached viewport 200 turns plus 1,000-line artifact",
        |benchmark| {
            benchmark.iter(|| {
                terminal
                    .draw(|frame| {
                        black_box(renderer.render(frame, &app, &theme));
                    })
                    .unwrap();
            });
        },
    );

    criterion.bench_function("transcript/100 alternating scroll frames", |benchmark| {
        benchmark.iter(|| {
            for _ in 0..50 {
                app.scroll_transcript_up();
                terminal
                    .draw(|frame| {
                        black_box(renderer.render(frame, &app, &theme));
                    })
                    .unwrap();
                app.scroll_transcript_down();
                terminal
                    .draw(|frame| {
                        black_box(renderer.render(frame, &app, &theme));
                    })
                    .unwrap();
            }
        });
    });

    criterion.bench_function("transcript/1,000 streamed deltas one frame", |benchmark| {
        benchmark.iter_batched(
            || {
                let mut app = App::new();
                app.screen = Screen::Chat;
                app.transcript.submit(1, "prompt".into(), Vec::new());
                app.handle_agent_event(AgentEvent::Started { request_id: 1 });
                (
                    app,
                    UiRenderer::default(),
                    Terminal::new(TestBackend::new(100, 30)).unwrap(),
                )
            },
            |(mut app, renderer, mut terminal)| {
                for _ in 0..1_000 {
                    app.handle_agent_event(AgentEvent::TextDelta {
                        request_id: 1,
                        text: "delta ".into(),
                    });
                }
                terminal
                    .draw(|frame| {
                        black_box(renderer.render(frame, &app, &theme));
                    })
                    .unwrap();
            },
            BatchSize::LargeInput,
        );
    });
}

fn composer_benchmarks(criterion: &mut Criterion) {
    cold_layout_size_matrix(criterion);
    paste_benchmarks(criterion);
    scale_and_edit_benchmarks(criterion);
    cached_layout_and_navigation_benchmarks(criterion);
    submission_preflight_benchmark(criterion);
    transcript_rendering_benchmarks(criterion);
    transcript_interaction_benchmarks(criterion);
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(1));
    targets = composer_benchmarks
}
criterion_main!(benches);
