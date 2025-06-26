#!/usr/bin/python3

import logging
import os
import pprint
import re
import shutil
import subprocess

# Globals
logger = logging.getLogger(__name__)

def parse_filename(curr_file: str) -> tuple[str,str,str,str,str,str]:
    """Parse file name to extract metadata.

    Args:
        curr_file (str): File to parse data from.

    Returns:
        tuple(str): Tuple holding the values parsed.
    """

    res = re.match(
        'goflow2_([0-9]{4})([0-9]{2})([0-9]{2})_([0-9]{2})([0-9]{2}).([A-Za-z0-9\.]*)',
        curr_file,
    )
    if not res:
        raise RuntimeError(f"Invalid file name")


    year    = res.group(1)
    month   = res.group(2)
    day     = res.group(3)
    hour    = res.group(4)
    minute  = res.group(5)
    f_ext   = res.group(6)

    return (year, month, day, hour, minute, f_ext)


def main() -> None:
    '''Main function'''

    # Start - params

    dir_base = '/home/<user>/goflow2/protobuf-archive'
    dir_archive = '/mnt/archive-disk/netflow-logs'
    dir_rel_logs = 'logs/'

    log_file_regex = 'goflow2_[0-9]+_[0-9]+.log'

    # These can possibly be read from the current environment as this script
    # and 99/100 scenarios will use the current user
    file_uid = 1000
    file_gid = 1000

    compress_bin = 'xz'
    compress_ext = 'xz'
    compress_params = ['-9','-e']

    # End - params

    # Ensure we are in the correct folder
    # Required for docker compose
    os.chdir(dir_base)

    # Set permissions on current files within container
    # Set permissions on base folder
    result = subprocess.run([
        'docker','compose','exec','goflow2','sh','-c',
        f'chown {file_uid}:{file_gid} /var/log/goflow/',
    ], capture_output=True)
    # Set permissions on log files we are about to archive
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
        try:
            logger.info(f"Processing: {curr_file}")

            year, month, day, hour, minute, f_ext = parse_filename(curr_file)
            logger.debug(f"File meta: {year} # {month} # {day} # {hour} # {minute} # {f_ext}")

            final_path = f"{dir_archive}/{year}/{month}/{day}"
            logger.debug(f"Dst path: {final_path}")

            # Create dst path if it doesn't exist
            os.makedirs(final_path, exist_ok=True)

            if f_ext == 'log':
                logger.info(f"Compressing: {curr_file}")
                subprocess.run([
                    compress_bin,
                    *compress_params,
                    f'logs/{curr_file}'
                ])
                # Update the current file extension
                curr_file += f'.{compress_ext}'
                f_ext     += f'.{compress_ext}'

            logger.info(f"Moving {curr_file} to {final_path}")
            shutil.move(f"logs/{curr_file}", f"{final_path}/{curr_file}")
        except Exception as exc:
            logger.error(f"Error processing '{curr_file}': {exc}")

    logger.debug("Done")

if __name__ == '__main__':
    logging.basicConfig(
        level=logging.INFO,
        format='%(asctime)s - %(name)s - %(levelname)s - %(message)s',
    )
    main()
