# 🚀 IDM Engine (Advanced Download Manager)

A blazing-fast, asynchronous, multi-threaded Internet Download Manager engine written in Rust. 
Originally built as a CLI tool, this project is now compiled as a **Shared Library (`.so` / `.dll`)** via FFI (Foreign Function Interface), allowing you to embed a hyper-optimized download engine into any programming language (Python, Node.js, C++, etc.).

**Author:** Shay Kadosh
**Version:** 2.0.0

---

## ✨ Key Features

* **Multi-Connection Downloading:** Splits the file and downloads using **8 concurrent workers** to maximize your internet bandwidth.
* **Smart Resume (State Saving):** Interruptions? No problem. The engine saves progress in a `.idm` state file. If the download stops or crashes, running it again will resume exactly from where it left off, skipping completed chunks.
* **Anti-Bot Bypass:** Uses a standard Chrome `User-Agent` to bypass `403 Forbidden` restrictions on strict servers.
* **Automatic Redirects:** Seamlessly follows up to 10 HTTP redirects (e.g., `301`, `302`) to find the actual file location.
* **Integrity Verification:** Automatically calculates and verifies the **SHA-256** checksum of the downloaded file upon completion.
* **Visual Progress:** If run in a terminal context, it utilizes `indicatif` to display a beautiful, single-line progress bar with real-time ETA and speed metrics.
* **Safe Overwrite:** Checks if a file already exists before starting to prevent accidental data loss.

---

## 🛠️ Building the Engine

To compile the engine into a shared library, ensure you have [Rust and Cargo](https://rustup.rs/) installed, then run the following command to build with maximum optimizations:

```bash
cargo build --release
