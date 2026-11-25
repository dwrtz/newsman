import {bindings, defineConfig, triggers} from "cf/config";

const name = process.env.NEWSMAN_WORKER_NAME || "newsman";
const queue = process.env.NEWSMAN_QUEUE || name + "-send";
const vars = ["PUBLIC_ORIGIN", "PUBLICATION_ORIGIN", "PUBLICATION_NAME", "ACCESS_TEAM_DOMAIN",
  "ACCESS_AUD", "BOOTSTRAP_OPERATORS", "OPERATOR_AUTH_MODE", "OPERATOR_EMAILS", "PILOT_RECIPIENTS", "AWS_ROLE_ARN", "OAUTH_CLIENTS", "OAUTH_CIMD_CLIENTS", "AWS_REGION", "SES_CONFIGURATION_SET",
  "SNS_TOPIC_ARN", "FROM_ADDRESS", "REPLY_TO", "ARCHIVE_FEED_URL", "TIME_ZONE", "ARCHIVE_LAG_DAYS"];
const secrets = ["NEWSMAN_SECRET", "READER_PROOF_SECRET", ...(process.env.AWS_ROLE_ARN ? ["AWS_WORKLOAD_KEY"] : ["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"])];
export default defineConfig({
  worker: {
    name, entrypoint: "./build/index.js", compatibilityDate: "2026-11-25",
    assets: {runWorkerFirst: true},
    observability: {enabled:true,redactQueryString:true,logs:{enabled:true,invocationLogs:false},traces:{enabled:false}},
    env: {
      DB: bindings.d1({name:process.env.NEWSMAN_DB || name,id:process.env.NEWSMAN_D1_ID}),
      FILES: bindings.r2({name:process.env.NEWSMAN_BUCKET || name + "-files"}),
      ASSETS: bindings.assets(), SEND_QUEUE: bindings.queue({name:queue}),
      ...Object.fromEntries(vars.filter(v=>process.env[v]).map(v=>[v,bindings.text(process.env[v]!)])),
      ...Object.fromEntries(secrets.map(v=>[v,bindings.secret()])),
      ...(process.env.NEWSMAN_AWS_TEMPORARY==="yes" ? {AWS_SESSION_TOKEN:bindings.secret()}:{}),
    },
    triggers: [triggers.scheduled({schedule:"* * * * *"}),triggers.queue({name:queue,deadLetterQueue:queue+"-dead",maxBatchSize:10,maxRetries:3})],
  }
});
