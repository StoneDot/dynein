## dy bootstrap

```
$ dy bootstrap --help
dy-bootstrap 0.2.1
Create sample tables and load test data for bootstrapping

USAGE:
    dy bootstrap [FLAGS] [OPTIONS]

FLAGS:
    -h, --help       Prints help information
    -l, --list       
    -V, --version    Prints version information

OPTIONS:
        --endpoint-url <endpoint-url>    Specify the endpoint to use (e.g. --endpoint-url http://dynamodb.us-east-
                                         2.amazonaws.com/). If you use this option with
                                         `--port`, the endpoint is rewritten by the value of`--port`. Stored config of
                                         port does not affect the specified endpoint. Please note that the endpoint's
                                         URL and the specified region should align
    -p, --port <port>                    Specify the port number. This option has an effect only when `--region local`
                                         is used
    -r, --region <region>                The region to use (e.g. --region us-east-1). When using DynamodB Local, use
                                         `--region local`. You can use --region option in both top-level and subcommand-
                                         level
    -s, --sample <sample>                
    -t, --table <table>                  Target table of the operation. You can use --table option in both top-level and
                                         subcommand-level. You can store table schema locally by executing `$ dy use`,
                                         after that you need not to specify --table on every command

$ dy help bootstrap
dy-bootstrap 0.2.1
Create sample tables and load test data for bootstrapping

USAGE:
    dy bootstrap [FLAGS] [OPTIONS]

FLAGS:
    -h, --help       Prints help information
    -l, --list       
    -V, --version    Prints version information

OPTIONS:
        --endpoint-url <endpoint-url>    Specify the endpoint to use (e.g. --endpoint-url http://dynamodb.us-east-
                                         2.amazonaws.com/). If you use this option with
                                         `--port`, the endpoint is rewritten by the value of`--port`. Stored config of
                                         port does not affect the specified endpoint. Please note that the endpoint's
                                         URL and the specified region should align
    -p, --port <port>                    Specify the port number. This option has an effect only when `--region local`
                                         is used
    -r, --region <region>                The region to use (e.g. --region us-east-1). When using DynamodB Local, use
                                         `--region local`. You can use --region option in both top-level and subcommand-
                                         level
    -s, --sample <sample>                
    -t, --table <table>                  Target table of the operation. You can use --table option in both top-level and
                                         subcommand-level. You can store table schema locally by executing `$ dy use`,
                                         after that you need not to specify --table on every command

```
