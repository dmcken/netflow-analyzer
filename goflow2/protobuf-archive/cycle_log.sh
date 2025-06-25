#!/bin/bash

cd /home/stech/goflow2-mine

docker compose exec goflow2 sh -c "mv /var/log/goflow/goflow2.log /var/log/goflow/goflow2_`date --utc +%Y%m%d_%H%M`.log && kill -1 1"
