# OpenBubbles RelayServer (64-Bit iOS / arm64)

> [!IMPORTANT]
> ### Upstream Attribution & Development Scope
> **The core relay logic, network protocol, and fundamental codebase are the intellectual property of [OpenBubbles](https://github.com/OpenBubbles/relayserver).**
> 
> This repository is **not** an independent fork and is **not** maintained as a divergent implementation. We do **not** work on the core relay logic independently. All core registration logic, cryptography, and protocol handling belong to and are driven by OpenBubbles upstream.
> 
> This repository provides 64-bit (`arm64`) platform enhancements and system integrations:
> - **Hardware Status Telemetry:** Local battery capacity and charging power state monitoring for headless operation.
> - **Home Assistant Push Integration:** Real-time push reporting of daemon health, pairing code, and battery state directly to Home Assistant REST API.
> - **Daemon Lifecycle & Singleton Control:** Port 8080 listener mutex to prevent concurrent instance conflicts under `launchd`.
> - **Interactive Web Dashboard:** Lightweight local web interface for pairing code inspection and dynamic configuration.
> - **Hardened Concurrency & Reliability:** Safe Mach IPC port management, atomic configuration file persistence, and automatic idle connection recovery.

---

## Hardware & Environment Support

| Parameter | Specification |
| :--- | :--- |
| **Supported Devices** | iPhone 5s through iPhone X, iPad Air, iPad mini 2+, iPad Pro |
| **Architecture** | 64-bit ARM (`arm64` / `aarch64-apple-ios`) |
| **Operating System** | iOS 10.0 through iOS 16.x (checkra1n, unc0ver, palera1n, Taurine, Dopamine) |
| **Core Service** | Background registration relay daemon |
| **Local Web Interface** | `http://<device-ip>:8080/` |

---

## Features Added in This Distribution

### 1. Hardware Status Telemetry
Extracts real-time hardware status for headless monitoring:
- Battery percentage reporting
- External power and charging state detection
- iOS version and device model dynamic discovery

### 2. Home Assistant Push Reporter
Periodically pushes hardware and service telemetry directly to your Home Assistant instance without requiring polling:
- `sensor.openbubbles_relay`: Service state (`Online`, `Connecting`)
- `sensor.openbubbles_relay_code`: Current 6-digit registration code
- `sensor.openbubbles_relay_battery`: Battery level percentage and charging state

Configuration is managed dynamically via `/var/mobile/config.json` or the local web UI at `http://<device-ip>:8080/`—**no secrets or tokens are hardcoded into the binary**.

### 3. Singleton Daemon Lock
Prevents duplicate instances from spawning and fighting over TCP socket 8080:
```rust
let listener = TcpListener::bind("0.0.0.0:8080").await
    .expect("Failed to bind port 8080: another instance is already running");
```

### 4. Hardened Network & State Management
- **Dead Wi-Fi Link Recovery:** 180-second idle heartbeat timeout recovers stale half-open connections.
- **Atomic Config Persistence:** Writes through temporary files and backups to eliminate configuration file corruption.
- **Redacted Diagnostics:** Sensitive tokens and credentials are masked in JSON outputs and logs.

---

## Installation via APT Repository

Add the community repository to your package manager of choice (Sileo, Zebra, or Cydia):

- **Repository URL:** `https://cydia.glenmuthoka.com/` (or `https://bananz0.github.io/cydia-repo/`)
- **Package:** `dev.copper.relayserver` (RelayServer)

### 1-Tap Links
- **Sileo:** `sileo://source/https%3A%2F%2Fcydia.glenmuthoka.com%2F`
- **Zebra:** `zbra://sources/add/https%3A%2F%2Fcydia.glenmuthoka.com%2F`
- **Cydia:** `cydia://url/https://cydia.saurik.com/api/share#?source=https%3A%2F%2Fcydia.glenmuthoka.com%2F`

---

## Credits & Upstream Links

- **Upstream OpenBubbles:** [github.com/OpenBubbles/relayserver](https://github.com/OpenBubbles/relayserver)
- **OpenBubbles Project:** [openbubbles.app](https://openbubbles.app)
- **32-Bit Repository:** [github.com/Bananz0/relayserver](https://github.com/Bananz0/relayserver)
- **64-Bit Repository:** [github.com/Bananz0/relayserver-arm64](https://github.com/Bananz0/relayserver-arm64)
- **APT Repository:** [github.com/Bananz0/cydia-repo](https://github.com/Bananz0/cydia-repo)
