import subprocess
import json

def get_root_sns():
    dfx = subprocess.Popen(
        [
            'dfx',
            'canister',
            '--network',
            'mainnet',
            'call',
            'sns-wasm',
            'list_deployed_snses',
            '(record {})'
        ], stdout=subprocess.PIPE
    )
    return json.loads(subprocess.check_output(['idl2json'], stdin=dfx.stdout).decode('utf-8').strip().split('\n')[0])['instances'][0]['root_canister_id']

def get_all_canisters(root_canister):
    dfx = subprocess.Popen(
        [
            'dfx',
            'canister',
            '--network',
            'ic',
            'call',
            '--candid',
            'sns_root.did',
            root_canister,
            'list_sns_canisters',
            '(record {})'
        ], stdout=subprocess.PIPE
    )
    return json.loads(subprocess.check_output(['idl2json'], stdin=dfx.stdout).decode('utf-8').strip().split('\n')[0])

def generate_source(canister):
    obj = dict()
    obj['type'] = 'sns_canister'
    obj['endpoint'] = f'https://{canister}.raw.mainnet.dfinity.network'
    obj['data_dir'] = 'logs'

    return obj

def generate_transform(canister, name, canister_type):
    obj = dict()
    obj['type'] = 'remap'
    obj['inputs'] = list()
    obj['inputs'].append(name)
    obj['source'] = f'.canister_type="{canister_type}"'
    
    return obj

def mock_get_root_sns():
    return ['5s2ji-faaaa-aaaaa-qaaaq-cai']

def main():
    root_sns = get_root_sns()
    config = dict()
    config['sources'] = dict()
    config['transforms'] = dict()
    
    for root in root_sns:
        canisters = dict(get_all_canisters(root))
        for key in canisters.keys():
            for canister in canisters[key]:
                name = f'{key}_{canister}'
                config['sources'][f'source_{name}'] = generate_source(canister)
                config['transforms'][f'transform_{name}'] = generate_transform(canister, f'source_{name}', key)

    print(json.dumps(config, indent=2))

if __name__ == "__main__":
    main()