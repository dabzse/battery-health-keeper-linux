# Development Guide

Quick reference for common development commands.

## Build & Run

| Command                 | Description                                              |
| :---------------------- | :------------------------------------------------------- |
| `cargo run`             | Compile and run the application in debug mode            |
| `cargo build`           | Compile the project without running it                   |
| `cargo build --release` | Compile with optimizations (slower build, faster binary) |

## Code Quality

| Command                | Description                                                                           |
| :--------------------- | :------------------------------------------------------------------------------------ |
| `cargo check`          | Fast type-check without producing a binary — catches compile errors quickly           |
| `cargo clippy`         | Run the Rust linter — warns about common mistakes and suggests idiomatic improvements |
| `cargo fmt`            | Auto-format all source files using `rustfmt` (applies changes in-place)               |
| `cargo fmt -- --check` | Check formatting without modifying files (useful in CI)                               |

## Testing

| Command                     | Description                                    |
| :-------------------------- | :--------------------------------------------- |
| `cargo test`                | Compile and run all tests (unit + integration) |
| `cargo test -- --nocapture` | Run tests and show `println!` output           |
| `cargo test <name>`         | Run only tests whose name contains `<name>`    |

## Dependencies

| Command             | Description                                               |
| :------------------ | :-------------------------------------------------------- |
| `cargo update`      | Update all dependencies to the latest compatible versions |
| `cargo tree`        | Display the full dependency tree                          |
| `cargo add <crate>` | Add a new dependency to `Cargo.toml`                      |

## Misc

| Command            | Description                                         |
| :----------------- | :-------------------------------------------------- |
| `cargo doc --open` | Generate API documentation and open it in a browser |
| `cargo clean`      | Remove the `target/` directory (compiled artifacts) |

## Typical Workflow

```bash
# 1. Quick check — does it compile?
cargo check

# 2. Lint — any warnings or style issues?
cargo clippy

# 3. Format — consistent code style
cargo fmt

# 4. Test — everything passes?
cargo test

# 5. Run
cargo run
```
