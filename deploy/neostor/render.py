#!/usr/bin/env python3
"""Render cache-only scrapes. No credentials, hardware requests or Agent config writes."""
import argparse
import ipaddress
import json
from pathlib import Path
import re


def target(value, port):
    host, sep, number = value.rpartition(':')
    address = ipaddress.IPv4Address(host)
    networks = ['10.0.0.0/8', '172.16.0.0/12', '192.168.0.0/16', '127.0.0.0/8']
    if not sep or number != str(port) or not any(address in ipaddress.ip_network(n) for n in networks):
        raise ValueError('Use a verified private IPv4 cache endpoint on port ' + str(port))
    return value


def render(rest, telemetry, site):
    if not re.fullmatch(r'[a-z][a-z0-9-]{0,62}', site):
        raise ValueError('A stable IDC routing code is required')
    jobs = []
    for name, endpoint, interval, timeout in [
        ('ufm-rest', target(rest, 9627), 60, 30),
        ('ufm-telemetry', target(telemetry, 9628), 180, 60),
    ]:
        jobs.append({
            'job_name': name, 'scrape_interval': str(interval) + 's',
            'scrape_timeout': str(timeout) + 's', 'sample_limit': 500000,
            'honor_labels': False, 'honor_timestamps': True,
            'static_configs': [{'targets': [endpoint], 'labels': {
                'cluster': site, 'host': 'ufm-fabric', 'device_type': 'ib_fabric',
                'interval_secs': str(interval), 'integration': 'ufm-cache',
            }}],
            'metric_relabel_configs': [
                # The center reserves source="neostor". Preserve 9001/9002 before
                # scoping, or these two diagnostics would become the same series.
                {'source_labels': ['source'], 'regex': '(.+)', 'target_label': 'ufm_source'},
                {'action': 'labeldrop', 'regex': 'source|site_code|idc|agent_id|tenant_id|device_id|monitoring_id|node_guid|port_guid'},
            ],
        })
    return {'scrape_configs': jobs}


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--rest-target', required=True)
    p.add_argument('--telemetry-target', required=True)
    p.add_argument('--site-code', required=True)
    p.add_argument('--output', type=Path, required=True)
    a = p.parse_args()
    config = render(a.rest_target, a.telemetry_target, a.site_code)
    # Do not replace reviewed live config implicitly; render to a staging path.
    with a.output.open('x') as f:
        json.dump(config, f, indent=2)
        f.write('\n')


if __name__ == '__main__':
    main()
