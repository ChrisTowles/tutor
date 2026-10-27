default:
    @just --list

# Build tutor and tutor-station (release); tutor deploys the station binary from beside itself
build:
    cargo build --release

# Follow today's event log on this PC
events:
    tail -n 50 -F ~/.local/share/tutor/telemetry/events-$(date -u +%F).jsonl | jq -c 'del(."service.version", ."process.pid", ."host.name", .kind)'

# Build, then start the tutor window
run *args: build
    ./target/release/tutor {{args}}
