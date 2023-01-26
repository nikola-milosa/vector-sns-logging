import requests
import subprocess
import json

def get_shards():
    response = subprocess.check_output(
        [
            "kubectl",
            "get",
            "po",
            "-n",
            "ic-logs-vector",
            "-o",
            "json"
        ]
    )
    response = json.loads(response)
    response = response['items']
    names = []
    for item in response:
        names.append(item['metadata']['name'])
    return names

def sources_per_shard(shard):
    response = subprocess.check_output(
        [
            "kubectl",
            "exec",
            shard,
            "-n",
            "ic-logs-vector",
            "-c",
            "log-config-generator",
            "--",
            "cat",
            "/generated-config/node_exporter.json"
        ]
    )

    response = list(json.loads(response)['sources'].keys())
    return list(map(lambda key: key.split('-node_exporter')[0], response))
    
def get_nodes_from_dashboard():
    return list(requests.get("https://dashboard.mainnet.dfinity.systems/api/proxy/registry/mainnet/nodes").json().keys())

def main():
    shards = get_shards()

    nodes = []
    for shard in shards:
        nodes = nodes + sources_per_shard(shard)

    print(f"Found {len(nodes)} in sd")

    dashboard_nodes = get_nodes_from_dashboard()
    print(f"Found {len(dashboard_nodes)} in dashboard")
    print(f"Difference between dashboard nodes and sd nodes:\n{list(set(dashboard_nodes) - set(nodes))}")
    print(f"Difference between sd nodes and dashboard nodes:\n{list(set(dashboard_nodes) - set(nodes))}")



if __name__ == "__main__":
    main()