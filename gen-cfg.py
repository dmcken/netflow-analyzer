#!/usr/bin/env python3

import os
import string

files = [
    'nfacctd.conf',
    'sql/create-db.sql',
]

# Copied from stack overflow to avoid external dependency:
# https://stackoverflow.com/questions/40216311/reading-in-environment-variables-from-an-environment-file
def get_env_data_as_dict(path: str) -> dict:
    with open(path, 'r') as f:
       return dict(tuple(line.replace('\n', '').split('=')) for line
                in f.readlines() if not line.startswith('#'))

# Load the env variables
env_data = get_env_data_as_dict('./.env')

substitutions = {}
for curr_env in env_data.keys():
    try:
        substitutions[curr_env] = env_data[curr_env]
    except KeyError:
        print(f"Please set env variable: {curr_env}")

# Substitute the env variables in the templates
for curr_file in files:
    template_file = f"{curr_file}.tpl"

    with open(curr_file,'w') as f_config, open(template_file, 'r') as f_template:
        template_str = f_template.read()
        template = string.Template(template_str).substitute(substitutions)
        f_config.write(template)
