#
# Make sure persisting the same table in different schema does not cause interference
#

> CREATE SCHEMA schema1;

> CREATE SCHEMA schema2;

> CREATE TABLE schema1.t1 (f1 TEXT);

> INSERT INTO schema1.t1 VALUES ('schema1');

> CREATE TABLE schema2.t1 (f1 TEXT);

> INSERT INTO schema2.t1 VALUES ('schema2');

#
# 
#
