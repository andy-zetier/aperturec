#!/usr/bin/env python3

"""
Requirements:
- Python 3.6+
- System dependencies:
  - git
  - cargo (Rust toolchain)
  - Xvfb
  - ffmpeg
  - glxgears
  - gnuplot (for generating graphs)
- Environment:
  - SSH access to gitlab.zetier.com for git clone
  - Sufficient disk space for building Rust projects
  - X11 environment
  - Permissions to create temporary directories
"""

import argparse
import os
import shutil
import subprocess
import tempfile
import time
import socket
import random
import uuid
import csv
from glob import glob
import datetime
from datetime import datetime
import statistics
import math

###############################################################################
# Utility Functions
###############################################################################

def cpu_list_from_count(count, allowed_cpus):
    if count is None:
        return None
    if count < 1:
        raise ValueError(f"CPU count must be >= 1, got {count}")
    if count > len(allowed_cpus):
        raise ValueError(
            f"CPU count {count} exceeds available affinity CPUs {len(allowed_cpus)}"
        )
    selected = allowed_cpus[:count]
    return ",".join(str(cpu) for cpu in selected)

def positive_int(value):
    try:
        parsed = int(value)
    except ValueError as exc:
        raise argparse.ArgumentTypeError(f"Expected integer, got {value!r}") from exc
    if parsed < 1:
        raise argparse.ArgumentTypeError(f"Expected positive integer, got {parsed}")
    return parsed

def apply_taskset(cmd, cpu_list, label):
    if cpu_list is None:
        return cmd
    if shutil.which("taskset") is None:
        raise RuntimeError("taskset is required for --client-cpus/--server-cpus but was not found")
    print(f"Limiting {label} to CPUs: {cpu_list}")
    return ["taskset", "-c", cpu_list] + cmd

def plan_cpu_lists(client_cpus, server_cpus):
    """
    Plan non-overlapping CPU lists for client and server when possible.
    Returns (client_cpu_list, server_cpu_list) as taskset-compatible strings.
    """
    if client_cpus is None and server_cpus is None:
        return None, None

    allowed = sorted(os.sched_getaffinity(0))
    if not allowed:
        raise ValueError("No CPUs available in current affinity set")

    if client_cpus is not None and server_cpus is not None:
        total = client_cpus + server_cpus
        if total <= len(allowed):
            client_list = cpu_list_from_count(client_cpus, allowed)
            server_list = cpu_list_from_count(server_cpus, allowed[client_cpus:total])
        else:
            print(
                "Warning: Not enough CPUs to fully separate client and server; "
                "falling back to overlapping CPU sets."
            )
            client_list = cpu_list_from_count(client_cpus, allowed)
            server_list = cpu_list_from_count(server_cpus, allowed)
    else:
        client_list = cpu_list_from_count(client_cpus, allowed)
        server_list = cpu_list_from_count(server_cpus, allowed)

    return client_list, server_list

def pick_random_port():
    """Bind to an ephemeral port and return it."""
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.bind(('127.0.0.1', 0))
    port = s.getsockname()[1]
    s.close()
    return port

def pick_random_display_num(low=100, high=999):
    """
    Return a random integer that we'll attempt to use as a display number.
    For a real production script, you might want to check whether that display
    is already in use. This is a naive approach.
    """
    return random.randint(low, high)

def generate_auth_token_file(test_dir):
    """
    Generate a random auth token, write it to a file, and return the file path.
    """
    token_path = os.path.join(test_dir, "auth_token.txt")
    auth_token = str(uuid.uuid4())
    with open(token_path, "w") as f:
        f.write(auth_token)
    return token_path

def combine_csv_files(input_pattern, output_file):
    """
    Combine multiple CSV files matching the input_pattern into a single output_file.
    """
    files = sorted(glob(input_pattern))
    if not files:
        print(f"No CSV files found for pattern: {input_pattern}")
        return

    fieldnames = set()
    rows = []

    # Collect all fieldnames and rows
    for file in files:
        with open(file, "r", encoding="utf-8") as f:
            reader = csv.DictReader(f)
            fieldnames.update(reader.fieldnames)
            rows.extend(reader)

    # The input CSVs use "--------" to represent blank fields, so we'll replace those with None
    for row in rows:
        for key in row:
            if row[key] == "--------":
                row[key] = None

    # Write the combined CSV
    with open(output_file, "w", encoding="utf-8", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=sorted(fieldnames))
        writer.writeheader()
        writer.writerows(rows)

def sleep_with_server_output(server_proc, duration):
    """
    Sleep for duration seconds while monitoring server output.
    Returns False if server died during sleep, True otherwise.
    """
    start_time = time.time()
    os.set_blocking(server_proc.stdout.fileno(), False)
    while time.time() - start_time < duration:
        # Check if server died
        if server_proc.poll() is not None:
            print(f"Server process died during sleep with exit code {server_proc.returncode}")
            return False

        time.sleep(0.1)  # Short sleep to prevent CPU spinning
    return True

def get_git_commit_info(repo_dir):
    """
    Get the current git commit hash and dirty state.
    """
    try:
        commit_hash = subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=repo_dir, universal_newlines=True
        ).strip()
        dirty_state = subprocess.check_output(
            ["git", "diff", "--shortstat"], cwd=repo_dir, universal_newlines=True
        ).strip()
        is_dirty = bool(dirty_state)
        return commit_hash, is_dirty
    except subprocess.CalledProcessError:
        return None, None

###############################################################################
# Build Functions
###############################################################################

def build_server(test_dir, profile):
    """
    Build aperturec-server using the requested cargo profile, twice.
    """
    for i in range(2):
        print("Building aperturec-server (attempt {})...".format(i+1))
        cmd = ["cargo", "build", "--profile", profile, "-p", "aperturec-server"]
        ret = subprocess.run(cmd, cwd=test_dir)
        if ret.returncode != 0:
            raise RuntimeError("Build of aperturec-server failed on attempt {}.".format(i+1))

def build_client(test_dir, profile):
    """
    Build aperturec-client-gtk3 using the requested cargo profile, twice.
    """
    for i in range(2):
        print("Building aperturec-client-gtk3 (attempt {})...".format(i+1))
        cmd = ["cargo", "build", "--profile", profile, "-p", "aperturec-client-gtk3"]
        ret = subprocess.run(cmd, cwd=test_dir)
        if ret.returncode != 0:
            raise RuntimeError("Build of aperturec-client-gtk3 failed on attempt {}.".format(i+1))

###############################################################################
# Server Management
###############################################################################

def start_server(
    test_dir,
    port,
    server_logs_dir,
    auth_token_path,
    server_tmp_dir,
    profile,
    server_cpu_list,
):
    """
    Start the aperturec-server process, redirecting stdout+stderr to PIPE
    so we can detect readiness. Returns the Popen object.
    """
    server_env = os.environ.copy()
    server_env["TMPDIR"] = server_tmp_dir
    server_env["RUST_BACKTRACE"] = "full"

    # Where the server will generate its TLS material
    tls_dir = os.path.join(test_dir, "tls")
    os.makedirs(tls_dir, exist_ok=True)

    server_metrics_path = os.path.join(test_dir, "server_metrics.csv")

    server_cmd = [
        "cargo", "run", "-p", "aperturec-server", "--profile", profile, "--",
        "-vvv",
        "--bind-address=127.0.0.1:{}".format(port),
        "--tls-save-directory={}".format(tls_dir),
        "--auth-token-file={}".format(auth_token_path),
        "--metrics-csv={}".format(server_metrics_path),
        "--screen-config=1920x1080",
        "--log-file-directory={}".format(server_logs_dir),
        "glxgears -fullscreen"
    ]
    server_cmd = apply_taskset(server_cmd, server_cpu_list, "server")
    print("\nStarting aperturec-server:")
    print(" ".join(server_cmd))

    # Start the server
    server_proc = subprocess.Popen(
        server_cmd,
        cwd=test_dir,
        env=server_env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        universal_newlines=True
    )
    return server_proc, tls_dir

def wait_for_server_ready(server_proc):
    """
    Read lines from server_proc until we see "Listening for client" or the process ends.
    Return True if server became ready, False otherwise.
    """
    while True:
        line = server_proc.stdout.readline()
        if not line:
            # Process might have exited
            break

        if "Listening for client" in line:
            print("Server is now up and listening.\n")
            return True

        # If server died, poll() won't be None
        if server_proc.poll() is not None:
            break

    return False

###############################################################################
# Xvfb Management
###############################################################################

def start_xvfb(display_num, xvfb_log_path):
    """
    Start Xvfb on :{display_num}, return the Popen object.
    """
    xvfb_cmd = [
        "Xvfb", ":%d" % display_num,
        "-screen", "0", "1920x1080x24",
        "-nolisten", "tcp",
        "-noreset"
    ]
    print("Starting Xvfb on display :{}...".format(display_num))

    xvfb_log_file = open(xvfb_log_path, "w", encoding="utf-8")

    xvfb_proc = subprocess.Popen(
        xvfb_cmd,
        stdout=xvfb_log_file,
        stderr=subprocess.STDOUT,
        universal_newlines=True
    )
    # Give Xvfb a moment to set up
    time.sleep(1)
    return xvfb_proc, xvfb_log_file

###############################################################################
# FFmpeg Management
###############################################################################

def start_ffmpeg(display_num, output_file, ffmpeg_log_path):
    """
    Start ffmpeg capturing from :display_num at 30 fps for `duration` seconds.
    Return the Popen object.
    """
    ffmpeg_cmd = [
        "ffmpeg",
        "-y",
        "-video_size", "1920x1080",
        "-framerate", "30",
        "-f", "x11grab",
        "-i", ":%d" % display_num,
        output_file
    ]
    print("Starting ffmpeg recording (30 fps)...")

    ffmpeg_log_file = open(ffmpeg_log_path, "w", encoding="utf-8")
    ffmpeg_proc = subprocess.Popen(
        ffmpeg_cmd,
        stdout=ffmpeg_log_file,
        stderr=subprocess.STDOUT,
        universal_newlines=True
    )
    return ffmpeg_proc, ffmpeg_log_file

###############################################################################
# Client Management
###############################################################################

def start_client(
    test_dir,
    port,
    client_log_path,
    auth_token_path,
    server_cert_path,
    client_tmp_dir,
    display_num,
    profile,
    client_cpu_list,
):
    """
    Start the aperturec-client under the chosen :display_num.
    Return the Popen object and an open log file handle.
    """
    client_env = os.environ.copy()
    client_env["TMPDIR"] = client_tmp_dir
    client_env["DISPLAY"] = ":%d" % display_num
    client_env["RUST_BACKTRACE"] = "full"
    client_env["WAYLAND_DISPLAY"] = ""

    client_log_file = open(client_log_path, "w", encoding="utf-8")

    client_metrics_path = os.path.join(test_dir, "client_metrics.csv")

    client_cmd = [
        "cargo", "run", "-p", "aperturec-client-gtk3", "--profile", profile, "--",
        "-vvv",
        "--fullscreen",
        "--auth-token-file={}".format(auth_token_path),
        "--additional-tls-certificates={}".format(server_cert_path),
        "--metrics-csv={}".format(client_metrics_path),
        "127.0.0.1:{}".format(port)
    ]
    client_cmd = apply_taskset(client_cmd, client_cpu_list, "client")

    print("Starting aperturec-client on DISPLAY=:{}, connecting to port {}:".format(
        display_num, port))
    print(" ".join(client_cmd))

    client_proc = subprocess.Popen(
        client_cmd,
        cwd=test_dir,
        env=client_env,
        stdout=client_log_file,
        stderr=subprocess.STDOUT,
        universal_newlines=True
    )
    return client_proc, client_log_file

###############################################################################
# Main Test Function
###############################################################################

def run_test_for_branch(
    branch,
    test_dir,
    duration,
    profile,
    skip_clone=False,
    clone_source=None,
    server_cpus=None,
    client_cpus=None,
):
    branch_dir = os.path.join(test_dir, branch)
    os.makedirs(branch_dir, exist_ok=True)

    if not skip_clone:
        if clone_source:
            print(f"\n=== Cloning local repo for branch '{branch}' into '{branch_dir}' ===")
            clone_cmd = ["git", "clone", "-b", branch, clone_source, branch_dir]
        else:
            print(f"\n=== Cloning remote repo for branch '{branch}' into '{branch_dir}' ===")
            clone_cmd = [
                "git", "clone", "-b", branch,
                "git@github.com:Zetier/aperturec.git",
                branch_dir
            ]
        print(" ".join(clone_cmd))
        ret = subprocess.run(clone_cmd)
        if ret.returncode != 0:
            print(f"ERROR: git clone failed for branch '{branch}'. Skipping.")
            return
        code_dir = branch_dir
    else:
        print(f"\n=== Using local repo in-place for branch '{branch}' ===")
        script_dir = os.path.dirname(os.path.abspath(__file__))
        code_dir = os.path.abspath(os.path.join(script_dir, ".."))

    # Get git commit info
    commit_hash, is_dirty = get_git_commit_info(code_dir)
    if commit_hash:
        print(f"Commit hash: {commit_hash}")
        print(f"Dirty state: {'dirty' if is_dirty else 'clean'}")
    else:
        print("Failed to get git commit info")

    # Build
    try:
        build_server(code_dir, profile)
        build_client(code_dir, profile)
    except RuntimeError as e:
        print("ERROR during build:", e)
        return

    # 3) Start server with random port and random auth token
    client_cpu_list, server_cpu_list = plan_cpu_lists(client_cpus, server_cpus)
    port = pick_random_port()
    auth_token_path = generate_auth_token_file(branch_dir)
    server_tmp_dir = tempfile.mkdtemp(prefix="aperturec_server_tmp_")

    server_logs_dir = os.path.join(branch_dir, "server_logs")
    server_proc, tls_dir = start_server(
        branch_dir,
        port,
        server_logs_dir,
        auth_token_path,
        server_tmp_dir,
        profile,
        server_cpu_list,
    )

    # Wait until we see "Listening for client"
    server_ready = wait_for_server_ready(server_proc)
    if not server_ready:
        print("Server never printed 'Listening for client'; exiting test.")
        # The server might have died, let's finalize:
        server_proc.kill()
        server_proc.wait()
        shutil.rmtree(server_tmp_dir, ignore_errors=True)
        return

    # Give server an idle period to observe baseline resource useage
    print("Giving server 30 seconds idle period...")
    if not sleep_with_server_output(server_proc, 30):
        shutil.rmtree(server_tmp_dir, ignore_errors=True)
        return

    # 4) Start Xvfb, ffmpeg, and the client
    display_num = pick_random_display_num()
    xvfb_log_path = os.path.join(branch_dir, "xvfb.log")
    xvfb_proc, xvfb_log_file = start_xvfb(display_num, xvfb_log_path)

    server_cert_path = os.path.join(tls_dir, "cert.pem")

    ffmpeg_log_path = os.path.join(branch_dir, "ffmpeg.log")
    recording_path = os.path.join(branch_dir, "client_recording.mp4")
    ffmpeg_proc, ffmpeg_log_file = start_ffmpeg(display_num, recording_path, ffmpeg_log_path)

    client_tmp_dir = tempfile.mkdtemp(prefix="aperturec_client_tmp_")
    client_log_path = os.path.join(branch_dir, "client.log")
    client_proc, client_log_file = start_client(
        branch_dir,
        port,
        client_log_path,
        auth_token_path,
        server_cert_path,
        client_tmp_dir,
        display_num,
        profile,
        client_cpu_list,
    )

    # 5) Let everything run for <duration> seconds
    print(f"Test running for {duration} seconds...")
    if not sleep_with_server_output(server_proc, duration):
        print("Server died during test period")

    print("Terminating client...")

    # Stop the client
    client_proc.terminate()
    try:
        client_proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        client_proc.kill()

    # Stop ffmpeg
    ffmpeg_proc.terminate()
    try:
        ffmpeg_proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        ffmpeg_proc.kill()

    # Stop Xvfb
    xvfb_proc.terminate()
    try:
        xvfb_proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        xvfb_proc.kill()

    # Give a cooldown period to observe the server reclaim resources
    print("Cooldown period for 90 seconds...")
    sleep_with_server_output(server_proc, 90)

    # Log if the server is alread dead
    if server_proc.poll() is not None:
        exit_code = server_proc.returncode
        print(f"Server process already exited with code {exit_code}")
    else:
        print("Terminating server...")
        # Stop the server
        server_proc.terminate()
        try:
            server_proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            server_proc.kill()

    # Close out logs
    xvfb_log_file.close()
    ffmpeg_log_file.close()
    client_log_file.close()

    # Combine metrics CSV files
    combine_csv_files(os.path.join(branch_dir, "client_metrics.*.csv"), os.path.join(branch_dir, "client_metrics.csv"))
    combine_csv_files(os.path.join(branch_dir, "server_metrics.*.csv"), os.path.join(branch_dir, "server_metrics.csv"))

    # Clean up /tmp dirs
    shutil.rmtree(server_tmp_dir, ignore_errors=True)
    shutil.rmtree(client_tmp_dir, ignore_errors=True)

    print("=== Finished test for branch '{}' ===".format(branch))

###############################################################################
# Graph Generation
###############################################################################

def generate_comparison_graphs(test_root):
    """Generate comparative graphs for all metrics across branches."""
    graphs_dir = os.path.join(test_root, "graphs")
    os.makedirs(graphs_dir, exist_ok=True)

    # Find all branch directories
    branch_dirs = [d for d in os.listdir(test_root) if os.path.isdir(os.path.join(test_root, d))]
    # Remove graphs directory because we don't want to compare it with the other branches
    branch_dirs.remove("graphs")

    for metric_source in ['client', 'server']:
        # Get all metrics (excluding timestamp) from first branch's CSV
        first_csv = os.path.join(test_root, branch_dirs[0], f"{metric_source}_metrics.csv")
        if not os.path.exists(first_csv):
            continue

        with open(first_csv) as f:
            reader = csv.DictReader(f)
            metrics = [field for field in reader.fieldnames if field != 'timestamp_rfc3339']

        for metric in metrics:
            escaped_metric = metric.replace("_", "\\_")
            # Create gnuplot script
            plot_script = os.path.join(graphs_dir, f"{metric_source}_{metric}.gnuplot")
            with open(plot_script, 'w') as f:
                f.write(f"""
set terminal png size 1200,800
set output '{os.path.join(graphs_dir, f"{metric_source}_{metric}.png")}'
set title '{metric_source.replace("_", " ").title()} {escaped_metric}'
set xlabel 'Seconds from start'
set ylabel '{escaped_metric}'
set grid
set key outside right
plot """)

                # Add plot command for each branch
                plot_commands = []
                for branch in branch_dirs:
                    csv_path = os.path.join(test_root, branch, f"{metric_source}_metrics.csv")
                    if not os.path.exists(csv_path):
                        continue

                    # Create temporary data file with normalized timestamps
                    temp_data = os.path.join(graphs_dir, f"{branch}_{metric_source}_{metric}.dat")
                    with open(csv_path) as csv_in, open(temp_data, 'w') as data_out:
                        reader = csv.DictReader(csv_in)
                        start_time = None
                        for row in reader:
                            # Replace Z with +0000 and parse with strptime
                            ts_str = row['timestamp_rfc3339'].replace('Z', '+0000')
                            ts = datetime.strptime(ts_str, '%Y-%m-%dT%H:%M:%S.%f%z')
                            if start_time is None:
                                start_time = ts
                            seconds = (ts - start_time).total_seconds()
                            if format(row.get(metric)):
                                data_out.write(f"{seconds} {row[metric]}\n")

                    plot_commands.append(f"'{temp_data}' title '{branch}' with lines")

                f.write(", ".join(plot_commands))

            # Run gnuplot
            subprocess.run(['gnuplot', plot_script])

            # Clean up temporary data files
            for branch in branch_dirs:
                temp_data = os.path.join(graphs_dir, f"{branch}_{metric_source}_{metric}.dat")
                try:
                    os.remove(temp_data)
                except FileNotFoundError:
                    pass

            # Clean up gnuplot scripts
            try:
                os.remove(plot_script)
            except FileNotFoundError:
                pass

    print(f"Generated comparison graphs in {graphs_dir}")

def print_top_changed_stats(test_root, branches, metric_sources=('client','server'), top_n=5):
    """
    For each source (client/server), read the combined CSV in each branch,
    compute the mean of each metric column (excluding timestamp), then
    calculate across-branch stddev and print the top_n highest-variance metrics.
    """
    for src in metric_sources:
        # load all CSVs into dict: metric -> [mean_for_branch_i,...]
        metrics_data = {}  # metric_name -> list of means
        for branch in branches:
            csv_path = os.path.join(test_root, branch, f"{src}_metrics.csv")
            if not os.path.exists(csv_path):
                continue
            with open(csv_path, newline='', encoding='utf-8') as f:
                reader = csv.DictReader(f)
                sums = {}
                counts = {}
                for row in reader:
                    for k,v in row.items():
                        if k == 'timestamp_rfc3339' or v in (None, '', '--------'):
                            continue
                        try:
                            val = float(v)
                        except (ValueError, TypeError):
                            continue
                        if not math.isfinite(val):
                            continue
                        sums.setdefault(k, 0.0)
                        counts.setdefault(k, 0)
                        sums[k] += val
                        counts[k] += 1
                # compute mean for this branch
                for metric, total in sums.items():
                    mean = total / counts[metric]
                    metrics_data.setdefault(metric, []).append(mean)
        # compute coefficient of variation for each metric (sd/mean)
        cvs = []
        for metric, values in metrics_data.items():
            if len(values) < 2:
                continue
            mean = sum(values) / len(values)
            if mean == 0:
                continue
            sd = statistics.stdev(values)
            cv = sd / mean
            cvs.append((cv, metric, mean, sd, values))
        # sort by CV descending
        cvs.sort(reverse=True, key=lambda x: x[0])
        print(f"\nTop {top_n} relatively-changing metrics for {src}:")
        # Build table: Metric | CV% | Mean | StdDev | <branch1> | <branch2> | ...
        headers = ["Metric", "CV%", "Mean", "StdDev"] + branches
        rows = []
        for cv, metric, mean, sd, vals in cvs[:top_n]:
            row = [
                metric,
                f"{cv*100:.1f}%",
                f"{mean:.2f}",
                f"{sd:.2f}",
            ] + [f"{v:.2f}" for v in vals]
            rows.append(row)
        # Compute column widths for aligned table
        col_widths = []
        for col_idx, h in enumerate(headers):
            max_data = max((len(r[col_idx]) for r in rows), default=0)
            col_widths.append(max(len(h), max_data))
        # Print header
        hdr = "  ".join(headers[i].ljust(col_widths[i]) for i in range(len(headers)))
        sep = "  ".join("-" * col_widths[i]           for i in range(len(headers)))
        print(hdr)
        print(sep)
        # Print each data row
        for r in rows:
            line = "  ".join(r[i].ljust(col_widths[i]) for i in range(len(headers)))
            print(line)

###############################################################################
# Main
###############################################################################

def main():
    parser = argparse.ArgumentParser(
        description="Automated ApertureC test with separate builds, auth token, TLS cert, etc."
    )
    parser.add_argument(
        '--branches',
        nargs='*',
        default=['main'],
        help="Branches to test. Defaults to ['main']."
    )
    parser.add_argument(
        '--duration',
        type=int,
        default=120,
        help="Duration (in seconds) for each test. Default 120."
    )
    parser.add_argument(
        "--use-local-repo",
        action="store_true",
        help="Use the local repository containing this script instead of cloning new branches."
    )
    parser.add_argument(
        "--clone-local",
        action="store_true",
        help="Clone from the local repository containing this script instead of the remote."
    )
    parser.add_argument(
        "--profile",
        default="release",
        help="Cargo profile to use for both build and run (e.g. release, dev, dev-optimized)."
    )
    parser.add_argument(
        "--client-cpus",
        type=positive_int,
        default=None,
        help="Limit client to N CPUs."
    )
    parser.add_argument(
        "--server-cpus",
        type=positive_int,
        default=None,
        help="Limit server to N CPUs."
    )
    args = parser.parse_args()

    # Determine which branches to test.
    # If using the local repo, ignore --branches and auto-detect the checked-out branch.
    if args.use_local_repo:
        # local repo root is one level up from this script
        script_dir = os.path.dirname(os.path.abspath(__file__))
        repo_dir = os.path.abspath(os.path.join(script_dir, ".."))
        try:
            current_branch = subprocess.check_output(
                ["git", "rev-parse", "--abbrev-ref", "HEAD"],
                cwd=repo_dir,
                universal_newlines=True,
            ).strip()
        except subprocess.CalledProcessError:
            raise RuntimeError("Failed to detect current git branch in local repo")
        print(f"Detected local branch: {current_branch}")
        branches = [current_branch]
    else:
        branches = args.branches

    # If requested, point clone_source at our local repo path; else None → use remote
    if args.clone_local:
        script_dir = os.path.dirname(os.path.abspath(__file__))
        clone_source = os.path.abspath(os.path.join(script_dir, ".."))
    else:
        clone_source = None

    timestamp = datetime.now().strftime("%Y%m%d_%H%M%S")
    test_id = f"test_{timestamp}"
    test_root = os.path.abspath(os.path.join("aperturec_tests", test_id))
    os.makedirs(test_root, exist_ok=True)

    for branch in branches:
        run_test_for_branch(
            branch,
            test_root,
            args.duration,
            args.profile,
            skip_clone=args.use_local_repo,
            clone_source=clone_source,
            server_cpus=args.server_cpus,
            client_cpus=args.client_cpus,
        )

    generate_comparison_graphs(test_root)

    if len(branches) > 1:
        print_top_changed_stats(test_root, branches)

    print("All requested branches tested.")

if __name__ == "__main__":
    main()
