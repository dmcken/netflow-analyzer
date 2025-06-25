#!/usr/bin/python3

import logging
import os
import pprint
import re
import shutil
import subprocess

# Globals
logger = logging.getLogger(__name__)


def main() -> None:
    '''Main function'''

    # Start - params

    dir_base = '/home/<user>/goflow2/protobuf-archive'
    dir_archive = '/mnt/archive-disk/netflow-logs'
    dir_rel_logs = 'logs/'

    log_file_regex = 'goflow2_[0-9]*_[0-9]*.log'

    # These can possibly be read from the current environment as this script
    # and 99/100 scenarios will use the current user
    file_uid = 1000
    file_gid = 1000

    # End - params

    os.chdir(dir_base)

    # Set permissions on current files within container
    result = subprocess.run([
        'docker','compose','exec','goflow2','sh','-c',
        f'chown {file_uid}:{file_gid} /var/log/goflow/{log_file_regex}',
    ], capture_output=True)


    # goflow2_20250612_2330.log
    log_files = list(filter(
        lambda x: re.match(log_file_regex, x),
        os.listdir('logs/'),
    ))
    log_files.sort()

    logger.debug(f"File list:\n{pprint.pformat(log_files)}")

    for curr_file in log_files:
        logger.info(f"Processing: {curr_file}")

        res = re.match(
                'goflow2_([0-9]{4})([0-9]{2})([0-9]{2})_([0-9]{2})([0-9]{2}).([A-Za-z0-9\.]*)',
            curr_file,
        )
        if not res:
            logger.error(f"Invalid filr name: {curr_file}")
            continue

        year    = res.group(1)
        month   = res.group(2)
        day     = res.group(3)
        hour    = res.group(4)
        minute  = res.group(5)
        f_ext   = res.group(6)

        logger.debug(f"{year} # {month} # {day} # {hour} # {minute} # {f_ext}")

        final_path = f"{dir_archive}/{year}/{month}/{day}"

        logger.debug(f"Path: {final_path}")

        os.makedirs(final_path, exist_ok=True)

        if f_ext == 'log':
            logger.info(f"Compressing: {curr_file}")
            subprocess.run(['bzip2','-9',f'logs/{curr_file}'])
            curr_file += '.bz2'
            f_ext += '.bz2'

        logger.info(f"Moving {curr_file} to {final_path}")
        shutil.move(f"logs/{curr_file}", f"{final_path}/{curr_file}")

    logger.debug("Done")

if __name__ == '__main__':
    logging.basicConfig(level=logging.INFO)
    main()
