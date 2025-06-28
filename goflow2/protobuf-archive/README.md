# Goflow2 protobuf raw archive

This is an implementation using no database and simply use raw files and *NIX utilities.

The goal is to provide an archive of flow data to respond to attack reports or otherwise trace connections through CG-NAT. The data can be mined for trends but the primary purpose is to provide an audit log.

## Setup

### Pre-requisites
* docker
* docker compose
* Long term storage location
* Collector router (currently Mikrotik supported)

### Local setup
1. Clone repo
2. `cd goflow2/protobuf-archive/`
3. Setup variables
4. `docker compose up -d`

### Crontab setup

```bash
*/30 * * * * /home/<user>/goflow2/protobuf-archive/cycle_log.sh >> /home/<user>/goflow2/protobuf-archive/cycle_log.log 2>&1

10 */3 * * * /home/<user>/goflow2/protobuf-archive/archive_logs.py >> /home/<user>/goflow2/protobuf-archive/archive_logs.log 2>&1
```

### Router setup

#### Mikrotik

```
/ip traffic-flow
set cache-entries=32M enabled=yes interfaces=<Set LAN ports you want to monitor> sampling-interval=1000000 sampling-space=1
/ip traffic-flow ipfix
set nat-events=yes
/ip traffic-flow target
add dst-address=<goflow2 host> src-address=<router lo> version=ipfix
```


## Compression comparison

Algorithms / formats:
* 7z
* bz2
* gzip
* lzip
* xz
* zip

Comparison (using a sample 506,284,009 file):
| Format | Parameters | Compressed size | Compression Percentage | Time elapsed |
| ------ | ---------- | ---------- | ---------- | ------------ |
| bzip2 | -1 | 72,907,925 | 14.4006% | 0:00:41.656107 |
| bzip2 | -5 | 65,580,484 | 12.9533% | 0:00:44.698288 |
| bzip2 | -9 | 64,166,159 | 12.6739% | 0:00:47.191346 |
| xz | -1 | 72,206,660 | 14.2621% | 0:00:01.829075 |
| xz | -1 , -e | 60,647,228 | 11.9789% | 0:00:13.669192 |
| xz | -5 | 57,316,688 | 11.3211% | 0:00:10.298091 |
| xz | -5 , -e | 52,593,904 | 10.3882% | 0:00:17.161286 |
| xz | -9 | 47,460,460 | 9.3743% | 0:01:44.994115 |
| xz | -9 , -e | 46,484,588 | 9.1815% | 0:01:57.072126 |
| 7z | -m0=lzma2 , -mx1 | 73,806,700 | 14.5781% | 0:00:00.996621 |
| 7z | -m0=lzma2 , -mx5 | 53,105,760 | 10.4893% | 0:00:30.633183 |
| 7z | -m0=lzma2 , -mx9 | 46,628,832 | 9.2100% | 0:01:23.556390 |
| 7z | -m0=lzma , -mx1 | 72,940,409 | 14.4070% | 0:00:13.307251 |
| 7z | -m0=lzma , -mx5 | 52,563,384 | 10.3822% | 0:01:51.273573 |
| 7z | -m0=lzma , -mx9 | 46,028,248 | 9.0914% | 0:02:42.188860 |
| 7z | -m0=PPMd , -mx1 | 64,163,937 | 12.6735% | 0:00:21.281377 |
| 7z | -m0=PPMd , -mx5 | 54,931,908 | 10.8500% | 0:00:24.611289 |
| 7z | -m0=PPMd , -mx9 | 52,264,124 | 10.3231% | 0:00:59.432927 |

## Notes

### Protobuf file format

* The binary file format is simply a list of protobuf records of variable length (exactly the same as the goflow2 json format).
* Each record begins with a protobuf varint which is the length of the record that follows it.
* `-transport.file.sep=` defines the end of record value
    * If not specified it defaults to '\n' (hex 0x0A)
    * If specified with no character (e.g. `-transport.file.sep=`) then no end of record character is written and the next record begins immediately.
* The raw protobuf IDs can be mapped to their real type available at [flow.proto](https://github.com/netsampler/goflow2/blob/main/pb/flow.proto)
* I prefer to read the raw IDs as I can then convert the raw bytes to its real representation when mapping to the final field (especially with my custom fields). In both python and rust this is simple to achieve to read the IDs and the 4 base data types (Varint, Fixed64, LengthDelimited, Fixed32) including any possible custom fields.

### Rotate log files in goflow2

Sending a `kill -1` or `kill -SIGHUP` to the goflow2 process will trigger it to close and re-open the log file [goflow2 issue](https://github.com/netsampler/goflow2/issues/3) . *NIX allows you to move an open file, allowing easy log rotation.

Log files are located in `/var/log/goflow/` in the container which is mapped to `logs/` in this folder.

Put together it looks like the following:
```bash
docker compose exec goflow2 sh -c "mv /var/log/goflow/goflow2.log /var/log/goflow/goflow2_`date --utc +%Y%m%d_%H%M`.log && kill -1 1"
```

### File permissions in container

Setting the permissions of the log files

```bash
docker compose exec goflow2 sh -c "chown 1000:1000 /var/log/goflow/"
docker compose exec goflow2 sh -c "chown 1000:1000 /var/log/goflow/goflow2_[0-9]*_[0-9]*.log"
```

Change 1000:1000 to your user as appropriate and you can then work on the files as if they are owned by your user (compress, move, etc).

