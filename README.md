# Cica

An agentic personal assistant that lives in your chat.

Cica brings Claude's capabilities to your messaging apps. It can hold conversations, answer questions, search the web, run commands, read and write files, and learn new skills over time.

## Features

- **Multi-channel**: Chat via Telegram or Signal
- **Multi-user**: Each user gets their own agent identity and memory, while skills are shared
- **Memory**: Remembers important things about you across conversations
- **Skills**: Extensible through custom skills you build together
- **Self-contained**: All dependencies are managed locally, nothing is installed globally

## Requirements

- macOS or Linux
- Claude Code subscription or Anthropic API key

## Getting Started

```bash
# Build
cargo build --release

# Run setup wizard
./target/release/cica init

# Start the assistant
./target/release/cica
```

## Usage

Once running, message your bot on Telegram or Signal. On first contact, you'll go through a quick pairing flow, then Cica will learn who it is and who you are.

```bash
# Approve a new user
cica approve <pairing-code>

# Show where data is stored
cica paths
```

## License

Licensed under either of Apache License, Version 2.0 or MIT license at your option.
