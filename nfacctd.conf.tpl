! nfacctd configuration
!
!
!
daemonize: true
pidfile: /var/run/nfacctd.pid
syslog: daemon
!
nfacctd_port: 2055
!
aggregate: src_host,dst_host,post_nat_ip_src,post_nat_ip_dst,src_port,post_nat_port_src,post_nat_port_dst,dst_port,proto,tos,nat_event
! interested in in and outbound traffic
!aggregate: src_host,dst_host
! on this network
!
! storage methods
plugins: pgsql
sql_table_version: 9
sql_db: pmacct
sql_host: localhost
sql_data: typed
sql_user: pmacct
sql_passwd: $POSTGRES_PMACCT_PW
sql_table: acct_v9
! refresh the db every minute
sql_refresh_time: 60
! reduce the size of the insert/update clause
sql_optimize_clauses: true
! accumulate values in each row for up to an hour
sql_history: 1h
! create new rows on the minute, hour, day boundaries
!sql_history_roundoff: mhd
! in case of emergency, log to this file
!sql_recovery_logfile: /var/lib/pmacct/nfacctd_recovery_log
sql_cache_entries: 9999991
!

