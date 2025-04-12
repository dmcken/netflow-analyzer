# Simple archiver of logs

Problem definition: To define the simplest example of saving flows to protobuf or JSON files (see the format option in the docker-compose.yaml file). As few dependencies as possible are required (preferably zero external services / environments).




## Crontab installation

```
*/30 * * * * /home/<user>/netflow-analyzer/goflow2/simple-archiver/cycle_log.sh >> /home/<user>/netflow-analyzer/goflow2/simple-archiver/cycle_log.log 2>&1
```

## Notes

* These files compress well
    * `bzip2 -9` - Goes from 15GB to 2.2GB.
* Done

## Protobuf notes

* The goflow2 records:
    * "Each protobuf message is prefixed by its varint length" - https://github.com/netsampler/goflow2/tree/main
* Extending a protobuf record:
    * https://protobuf.dev/getting-started/pythontutorial/#extending-a-protobuf
* Done
