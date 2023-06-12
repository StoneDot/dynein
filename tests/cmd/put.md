## dy put

```
$ dy put --help
dy-put 0.2.1
Create a new item, or replace an existing item. [API: PutItem]

USAGE:
    dy put [OPTIONS] <pval> [sval]

FLAGS:
    -h, --help       Prints help information
    -V, --version    Prints version information

OPTIONS:
        --endpoint-url <endpoint-url>    Specify the endpoint to use (e.g. --endpoint-url http://dynamodb.us-east-
                                         2.amazonaws.com/). If you use this option with
                                         `--port`, the endpoint is rewritten by the value of`--port`. Stored config of
                                         port does not affect the specified endpoint. Please note that the endpoint's
                                         URL and the specified region should align
    -i, --item <item>                    Additional attributes put into the item, which should be valid JSON. e.g.
                                         --item '{"name": "John", "age": 18, "like": ["Apple", "Banana"]}'
    -p, --port <port>                    Specify the port number. This option has an effect only when `--region local`
                                         is used
    -r, --region <region>                The region to use (e.g. --region us-east-1). When using DynamodB Local, use
                                         `--region local`. You can use --region option in both top-level and subcommand-
                                         level
    -t, --table <table>                  Target table of the operation. You can use --table option in both top-level and
                                         subcommand-level. You can store table schema locally by executing `$ dy use`,
                                         after that you need not to specify --table on every command

ARGS:
    <pval>    Partition Key of the target item
    <sval>    Sort Key of the target item (if any)

$ dy help put
dy-put 0.2.1
Create a new item, or replace an existing item. [API: PutItem]

USAGE:
    dy put [OPTIONS] <pval> [sval]

FLAGS:
    -h, --help       Prints help information
    -V, --version    Prints version information

OPTIONS:
        --endpoint-url <endpoint-url>    Specify the endpoint to use (e.g. --endpoint-url http://dynamodb.us-east-
                                         2.amazonaws.com/). If you use this option with
                                         `--port`, the endpoint is rewritten by the value of`--port`. Stored config of
                                         port does not affect the specified endpoint. Please note that the endpoint's
                                         URL and the specified region should align
    -i, --item <item>                    Additional attributes put into the item, which should be valid JSON. e.g.
                                         --item '{"name": "John", "age": 18, "like": ["Apple", "Banana"]}'
    -p, --port <port>                    Specify the port number. This option has an effect only when `--region local`
                                         is used
    -r, --region <region>                The region to use (e.g. --region us-east-1). When using DynamodB Local, use
                                         `--region local`. You can use --region option in both top-level and subcommand-
                                         level
    -t, --table <table>                  Target table of the operation. You can use --table option in both top-level and
                                         subcommand-level. You can store table schema locally by executing `$ dy use`,
                                         after that you need not to specify --table on every command

ARGS:
    <pval>    Partition Key of the target item
    <sval>    Sort Key of the target item (if any)

```
