# Simple archiver of logs






## Crontab installation

```
*/30 * * * * /home/<user>/netflow-analyzer/goflow2/simple-archiver/cycle_log.sh >> /home/<user>/netflow-analyzer/goflow2/simple-archiver/cycle_log.log 2>&1
```



## Protobuf notes

* Extending a protobuf record:
    * https://protobuf.dev/getting-started/pythontutorial/#extending-a-protobuf
