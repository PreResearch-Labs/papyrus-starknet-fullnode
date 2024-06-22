import subprocess
import time
import json
import signal
import os
import shutil
import sys


def run_node(command):
    try:
        process = subprocess.Popen(command, shell=True,preexec_fn=os.setsid, text=True)
        return process
    except Exception as e:
        print(f"Failed to start command: {command}\nError: {e}")
        sys.exit(1)


def run_curl_on_client_node_monitoring():
    result = subprocess.run(
        "curl -X GET http://localhost:8082/monitoring/metrics",
        capture_output=True,
        text=True,
        shell=True,
    )
    return result.stdout


def extract_state_marker_from_monitoring_output(curl_output):
    try:
        lines = curl_output.strip().split("\n")
        for line in lines:
            if line.startswith("papyrus_state_marker"):
                papyrus_state_marker = int(line.strip().split()[-1])
                return papyrus_state_marker
        return None
    except Exception as e:
        print(f"Error parsing curl -X GET http://localhost:8082/monitoring/metrics output: {e}")
        sys.exit(1)


def terminate_process_group(pgid):
    try:
        os.killpg(pgid, signal.SIGTERM)
    except OSError:
        pass


def main():
    if len(sys.argv) != 2:
        print("Usage: python3 scripts/p2p_syncing_test/main.py <BASE_LAYER_NODE_URL>")
        sys.exit(1)

    base_layer_node_url = sys.argv[1]

    client_node_command = f"target/release/papyrus_node --base_layer.node_url {base_layer_node_url} --config_file scripts/p2p_syncing_test/client_node_config.json"
    server_node_command = f"target/release/papyrus_node --base_layer.node_url {base_layer_node_url} --config_file scripts/p2p_syncing_test/server_node_config.json"

    # run the commands in parallel
    client_node = run_node(client_node_command)
    server_node = run_node(server_node_command)

    time.sleep(15)
    try:
        curl_output = run_curl_on_client_node_monitoring()

        papyrus_state_marker = extract_state_marker_from_monitoring_output(curl_output)
        assert (
            papyrus_state_marker is not None
        ), "Failed to extract state marker value from monitoring output."
        assert (
            papyrus_state_marker >= 10
        ), f"papyrus_state_marker value is less than 10, papyrus_state_marker {papyrus_state_marker}. Failing CI."
    finally:
        terminate_process_group(os.getpgid(client_node.pid))
        terminate_process_group(os.getpgid(server_node.pid))


if __name__ == "__main__":
    main()
