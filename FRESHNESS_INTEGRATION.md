# Freshness Check Integration

## Overview
Freshness checking is opt-in. Each mirror reached by the existing map-aware explorer first receives the normal `.files` speed test. If it passes, its `.db` is downloaded in the same initial check for package freshness. Every verified candidate is scored before the weighted top-N shortlist is chosen for a serial `.files` speed retest. Its `.db` is not downloaded again. With freshness disabled, the upstream speed-only flow and top-N retest remain in place.

## Key Changes

### 1. Configuration (`src/config.rs`)
Added freshness configuration fields:
- `freshness_check: FreshnessMode` - Off by default; bare `--freshness-check` enables it with speed priority 1, while `--freshness-check=0.9` sets a custom 0–1 speed priority
- `ref_local_dir: String` - Path to local reference database directory (default: /var/lib/pacman/sync)
- `freshness_timeout: u64` - Timeout for freshness downloads in milliseconds (default: 15000)

### 2. Target Configurations
All pacman-based target configs now use `base_path` instead of `path_to_test`:
- **Supported targets with freshness**: archlinux, archarm, archlinuxcn, artix, blackarch, cachyos, chaotic, endeavouros, manjaro, rebornos
- **Unsupported targets** (use `path_to_test`): stdin, openbsd, arcolinux

For supported targets:
- Speed test file: `{base_path}.files`
- Freshness DB file: `{base_path}.db`

Example: `extra/os/x86_64/extra` → `extra/os/x86_64/extra.files` and `extra/os/x86_64/extra.db`

### 3. Mirror Structure (`src/mirror.rs`)
Added `base_path: Option<String>` field to the `Mirror` struct. This field:
- Contains the base path for supported mirrors (e.g., "extra/os/x86_64/extra")
- Set to `Some(...)` for pacman-based mirrors
- Set to `None` for unsupported mirrors (stdin, openbsd, arcolinux)

### 4. Freshness Module (`src/freshness.rs`)
New module providing:
- `check_mirror()` - Async function to check mirror freshness by comparing package build dates
- `FreshnessCheckResult` - Result structure containing score, packages compared, and optional error
- `PackageBuildDates` - Structure for parsed package timestamps
- Streaming database parsing for zstd, gzip, and tar formats; compressed input is not expanded into a second full-database buffer
- Package identity comes from each description's `%NAME%` field, so versioned archive directory names do not break comparisons
- Local score: average signed build-date difference in days against the local DB (equal = 0, newer > 0, older < 0)
- Peer score: average signed build-date difference in days against the newest build date seen among the local DB and checked candidates (newest = 0, older < 0)
- Record the newest `%BUILDDATE%` in each checked package database and display it in UTC
- Missing packages have no meaningful age and reduce freshness quality through package coverage
- Rejects HTML responses, HTTP errors, empty databases, and oversized downloads

The latest package build is only a proxy for the latest sync, not proof that the entire repository has synced. Including the local DB in the per-package frontier means every checked mirror can score below 1 when all checked mirrors lag behind the local DB.

Each matching package contributes `(mirror_build_date - reference_build_date) / 86400`; these differences are averaged over matching packages. Thus +1 is one day newer, -1 is one day older, +0.1 is 2.4 hours newer, and -1/24 is one hour older. Freshness quality is `coverage / (1 + max(0, -average_peer_age_days))`, where coverage is the fraction of frontier packages present. The initial speed used for shortlist selection is `(files_bytes + db_bytes) / (files_transfer_time + db_transfer_time)`, normalized by the fastest verified mirror. Final score is `speed_weight * speed_quality + (1 - speed_weight) * freshness_quality`. At weight 1, freshness breaks speed ties; at weight 0, speed breaks freshness ties. After retesting, final speed uses the new `.files` measurement for retested mirrors and their original `.files` measurement for unretested mirrors, so the basis is consistent. Setting `--top-mirrors-number-to-retest=0` keeps the initial combined speeds for all mirrors.

### 5. Speed Test Integration (`src/speed_test.rs`)
Extended `SpeedTestResult` with freshness fields:
- `freshness_score: Option<f64>`
- `local_missing_packages: Option<usize>`
- `relative_freshness_score: Option<f64>`
- `freshness_quality: Option<f64>`
- `relative_missing_packages: Option<usize>`
- `relative_lag_seconds: Option<u64>`
- `latest_build_date: Option<i64>`
- `freshness_packages_compared: Option<usize>`
- `freshness_error: Option<String>`

Freshness checking in the initial pass:
1. Reuse one HTTP client across `.files` and `.db` requests (connection reuse depends on server behavior), and combine their transfer measurements for speed in freshness mode
2. Parse the local reference DB once per repo path; after each successful `.files` speed probe, check its `.db` with at most eight concurrent DB downloads
3. Build a per-package frontier from the local DB and all successfully checked candidates, including packages absent from the local reference
4. Compute weighted scores across all verified candidates before selecting a serial retest shortlist; exclude candidates whose DB checks fail
5. Retest the top N with `.files` only, retain their DB-derived freshness, and re-rank every remaining candidate by freshness and comparable `.files` speed

## Workflow

1. **Initial Check**: Mirrors reached through the existing geographic exploration get a `.files` speed test. With opt-in freshness, each successful speed test is followed by a `.db` download and comparison.

2. **Final Phase**:
   - With freshness enabled, all verified candidates are scored together, then the top N by weighted score receive a serial `.files` retest; no second `.db` request is made
   - If N is 0, the retest is skipped and the initial combined speeds are used for final ordering (the exploration benchmark uses this setting)
   - Targets without freshness support, or runs with freshness disabled, retain the original top-N `.files` re-test
   - If no candidates pass freshness checking, the run fails without replacing the saved mirrorlist

3. **Final Ordering**:
   - **WITH freshness**: sorted by the user-selected weighted score using new retest speeds where available
   - **WITHOUT freshness**: sorted by speed only (original behavior)

## Usage

### Default (speed only):
```bash
rate-mirrors arch
```

### Enable freshness checking with speed priority 1:
```bash
rate-mirrors --freshness-check arch
```

### Enable freshness checking with custom priority:
```bash
rate-mirrors --freshness-check=0.1 arch
```

### Custom reference directory:
```bash
rate-mirrors --ref-local-dir=/custom/path arch
```

### Custom timeout:
```bash
rate-mirrors --freshness-timeout=20000 arch  # 20 seconds
```

## Dependencies Added
- `tar = "0.4"` - TAR archive parsing
- `zstd = "0.13"` - ZSTD decompression
- `flate2 = "1.0"` - GZIP decompression

## Implementation Notes

1. **Opt-in for supported targets**: Freshness checks run only with `--freshness-check[=WEIGHT]` (or its environment variable). Existing mirror discovery still uses geography and speed even at `--freshness-check=0`.

2. **Fail closed**: Mirrors with failed freshness checks are excluded. If all fail, no saved mirrorlist is replaced.

3. **Parallel execution**: Freshness checks use tokio tasks with an eight-download concurrency limit. Opt-in mode downloads `.db` for every speed-verified mirror reached by exploration, which can use significant bandwidth.

4. **Database format support**: Handles zstd-compressed, gzip-compressed, and raw tar archives, matching pacman's database formats. Downloads are limited to 64 MiB, streamed decompression to 256 MiB, and individual descriptions to 1 MiB.

5. **CachyOS variant support**: The `base_path` system preserves CachyOS's multi-architecture support (x86_64, x86_64-v3, x86_64-v4) through its existing wrapper logic.

6. **No changes to initial filtering**: Mirror selection and completion/delay filters remain in target fetch implementations (unchanged from original).

## Testing

Run focused offline tests:
```bash
cargo test --offline
```

To test freshness functionality:
```bash
# Test with limited mirrors and freshness enabled
./target/release/rate_mirrors --freshness-check --max-jumps=2 --country-test-mirrors-per-country=5 arch
```

## Future Enhancements

1. Cache freshness results to avoid repeated downloads
2. Verify downloaded package databases against repository signatures before trusting new package names
3. Support additional database formats (e.g., sqlite)
4. Expose freshness metadata in machine-readable output formats
