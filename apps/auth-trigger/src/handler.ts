import type { PreAuthenticationTriggerEvent, PreAuthenticationTriggerHandler } from "aws-lambda";
import { DynamoDBClient } from "@aws-sdk/client-dynamodb";
import { DynamoDBDocumentClient, GetCommand } from "@aws-sdk/lib-dynamodb";
import { SSMClient, GetParameterCommand } from "@aws-sdk/client-ssm";

const ddb = DynamoDBDocumentClient.from(new DynamoDBClient({}));
const ssm = new SSMClient({});
const tableName = process.env.TABLE_NAME!;
const clientMapParam = process.env.CLIENT_MAP_PARAM!;

let clientMap: Record<string, string> | null = null;

const getClientMap = async (): Promise<Record<string, string>> => {
  if (clientMap) return clientMap;
  const result = await ssm.send(new GetParameterCommand({ Name: clientMapParam }));
  clientMap = JSON.parse(result.Parameter?.Value ?? "{}") as Record<string, string>;
  return clientMap;
};

export const handler: PreAuthenticationTriggerHandler = async (
  event: PreAuthenticationTriggerEvent
) => {
  const clientId = event.callerContext.clientId;
  const username = event.userName;

  // Seeded admin user always passes
  if (username === "chris") {
    return event;
  }

  const map = await getClientMap();
  const appKey = map[clientId];
  if (!appKey) {
    throw new Error("Unknown application");
  }

  const result = await ddb.send(
    new GetCommand({
      TableName: tableName,
      Key: { username }
    })
  );

  const record = result.Item;
  if (!record) {
    throw new Error("Access denied");
  }

  const apps = record.apps as Record<string, string> | undefined;
  if (!apps || !apps[appKey]) {
    throw new Error("Access denied");
  }

  return event;
};
