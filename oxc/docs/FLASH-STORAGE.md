# Flash storage: the expert sidecar on an unencrypted partition

The Flash engine (`oxe-server --engine flash`) runs a 125B mixture-of-experts model that does not fit in
RAM. During decode it streams the experts it needs from one repacked file, the **expert sidecar**
`experts-r4.bin`, with a `.json` manifest beside it. That makes SSD read latency part of every decoded
token. On the test laptop the sidecar therefore lives on a **plain, unencrypted ext4 partition**. The
main system stays encrypted (LUKS + btrfs).

## Rule: only public model weights go there

The unencrypted partition holds **only public model weights**: the expert sidecar, its manifest and,
optionally, other files you could download again from a public source. Everything private stays on the
encrypted volume:

- prompts, sessions and chat history;
- the prefix, KV and state caches (the controller and the engine refuse a Flash prefix store inside
  the sidecar's directory);
- request ledgers, logs and receipts;
- configs and API keys.

## Why: measured on the test laptop

These are lab measurements from fio on the same NVMe drive, with 2 MiB random reads. They are not a public
receipt.

| Path | Throughput (QD5) | Mean completion (QD5) | CPU per GB read |
|---|---|---|---|
| Raw device (plain partition) | 3.74 GB/s | 2.65 ms | ~0.10 core-s/GB |
| LUKS volume | 2.41 GB/s | 4.1 ms | ~0.48 core-s/GB |

- Decryption costs about **0.4 core-seconds per GB** read. That work lands on the cores that serve the IO.
  Flash keeps those cores free of compute threads, because any compute next to the IO thread raises
  read latency.
- Expert reads are **random records of about 1.5 to 2.6 MiB**. A decode step waits for the experts it is
  missing, so it is **latency**, not bandwidth, that sets the decode stall.

## How to reproduce

Assumptions: a free region on the internal NVMe drive of at least the sidecar's size plus headroom
(the test laptop uses 150 GiB), root access once, and a Linux user who runs the engine. Replace
`/dev/nvme0n1pX` with your partition. Double-check it: formatting destroys its contents.

```sh
# 1. Create the partition (any tool: gdisk, parted, GNOME Disks), then format it as plain ext4
sudo mkfs.ext4 -L localai-models /dev/nvme0n1pX

# 2. Mount point, owned by the user who runs the engine
sudo mkdir -p /var/lib/local-ai-models
echo "UUID=$(sudo blkid -s UUID -o value /dev/nvme0n1pX) /var/lib/local-ai-models ext4 noatime,nofail 0 2" \
  | sudo tee -a /etc/fstab
sudo systemctl daemon-reload && sudo mount /var/lib/local-ai-models
sudo chown "$USER:" /var/lib/local-ai-models && chmod 700 /var/lib/local-ai-models
mkdir -p /var/lib/local-ai-models/flash
```

`noatime` avoids a metadata write on every read. `nofail` lets the system boot when the partition is
missing; Flash then falls back as described below.

```sh
# 3. Build the sidecar there, or move an existing one. Keep the manifest next to it.
#    (Build: REPRODUCE.md section 3, "Flash expert sidecar".)
ls -l /var/lib/local-ai-models/flash/experts-r4.bin /var/lib/local-ai-models/flash/experts-r4.bin.json
```

4. **Point the runtime at it.** In the controller's runtime config (see the
   [example](../examples/runtime/llm-runtime.json)), set the Flash entry's `--flash-sidecar` argument and
   its `sidecar_path` to `/var/lib/local-ai-models/flash/experts-r4.bin`, and `sidecar_bytes` to the file's
   size. The stage tool (`tools/stage_flash_oxe_production.py --sidecar ...`) writes both for you. When you
   start the engine by hand, pass `--flash-sidecar /var/lib/local-ai-models/flash/experts-r4.bin`. Without the
   flag, `oxe-server --engine flash` and the `flash-m3` tools pick that path by themselves when the file
   and its manifest are both there. Otherwise they fall back to
   `~/.cache/local-ai/snapdragon-engine/flash/experts-r4.bin`.

The engine checks the manifest's identity against the model at load, so a stale or mismatched copy is
refused, not used.

## Fallback and options

- **It works on an encrypted volume too, just slower.** Without the plain partition, keep the sidecar
  in the default location under `~/.cache/local-ai/snapdragon-engine/flash/` or anywhere on the LUKS
  volume, and point `--flash-sidecar` / `sidecar_path` there. Expect higher read latency and more CPU
  time on the IO cores (table above).
- **Lightly encrypted.** If your policy requires encryption at rest even for public weights, give the
  sidecar its own dm-crypt volume and pick the fastest cipher your CPU accelerates
  (`cryptsetup benchmark`). The cost falls between the two rows above. It has not been measured here.
- Other drives: any NVMe works. Throughput and latency then depend on that drive, so re-measure before
  you compare numbers.
