#!/usr/bin/env python3
import argparse
import requests
import urllib.parse
import datetime
import json
import logging
import os
import time

logging.basicConfig(level=logging.INFO, format='[%(asctime)s][%(levelname)s]: %(message)s', datefmt='%m/%d/%Y %I:%M:%S %p')

parser = argparse.ArgumentParser(description="Initialize cursors from loki")
parser.add_argument("loki_url", type=str, help="Http url which should be queried for cursors, shoud contain contain only the schema, host and port i.e http://localhost:3100/")
parser.add_argument("output_dir", type=str, help="Top directory in which vector should store folders with cursors")
parser.add_argument("--look-back", type=int, help="The amount of days to look back for a log entry. This should be set to atleast log retention period", default=10, dest="look_back")

args = parser.parse_args()

def main():
    loki_url = args.loki_url
    look_back = args.look_back 
    output_dir = args.output_dir
    if look_back * 24 > 721:
        logging.error('Invalid look-back. Maximum allowed by loki is 721h')
        exit(1)

    while True:
        response = requests.get("{}ready".format(loki_url))
        if response.status_code == 200:
            break
        
        logging.warning("Waiting for loki to be ready... Backing off")
        time.sleep(0.5)


    response = requests.get("{}loki/api/v1/label/ic_node/values".format(loki_url))

    if response.status_code != 200:
        print('Status code was not 200')
        logging.warning("Couldn't fetch the value's of 'ic-node' label in loki... Skipping")
        exit(0)

    nodes = response.json()['data']
    logging.info('Retreived the list of nodes. Found {} nodes'.format(len(nodes)))

    ## Ensure that, if there is something we pick it up, this should be set to maximum between the accepted look-back for loki and the retention period
    prev_year = datetime.date.today() - datetime.timedelta(look_back)

    for node in nodes:
        logging.info('Processing node {}'.format(node))
        argument = "{ic_node = \"" + node + "\"}"
        full_url = "{}loki/api/v1/query_range?query={}&limit=1&direction=backward&start={}".format(loki_url, urllib.parse.quote(argument), prev_year.strftime("%s"))
        response = requests.get(full_url)

        if response.status_code != 200:
            logging.warning('Negative response...\n{}'.format(str(response)))
            continue

        result = response.json()['data']['result']
        if len(result) == 0:
            logging.warning('Empty result... Skipping')
            continue

        cursor = json.loads(result[0]['values'][0][1])['__CURSOR']
        
        path = os.path.join(output_dir, "{}-node_exporter-source".format(node))
        if not os.path.exists(path):
            os.mkdir(path)
        else:
            logging.warning("Directory already exists, maybe this shouldn't be overriden? {}".format(node))

        checkpointer = os.path.join(path, "checkpoint.txt")
        with open(checkpointer, "w", encoding="utf-8") as f:
            f.write(cursor)

    logging.info("Finished initializing cursors")
    exit(0)


if __name__ == "__main__":
    main()