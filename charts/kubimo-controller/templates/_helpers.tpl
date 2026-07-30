{{/*
Expand the name of the chart.
*/}}
{{- define "kubimo-controller.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Create a default fully qualified app name.
We truncate at 63 chars because some Kubernetes name fields are limited to this (by the DNS naming spec).
If release name contains chart name it will be used as a full name.
*/}}
{{- define "kubimo-controller.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{/*
Create chart name and version as used by the chart label.
*/}}
{{- define "kubimo-controller.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Common labels
*/}}
{{- define "kubimo-controller.labels" -}}
helm.sh/chart: {{ include "kubimo-controller.chart" . }}
{{ include "kubimo-controller.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{/*
Selector labels
*/}}
{{- define "kubimo-controller.selectorLabels" -}}
app.kubernetes.io/name: {{ include "kubimo-controller.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/*
Fail the render if the drain cannot finish inside the pod's grace period.

Getting this wrong is silent and harmful: kubelet SIGKILLs the agent part-way through
the drain, leaving some slots flushed and unmounted and others abandoned on a volume that
is about to detach. Refusing to render beats discovering it during an upgrade.
*/}}
{{- define "kubimo-controller.validateDrain" -}}
{{- $drain := .Values.agent.drain -}}
{{- if not (gt (int $drain.terminationGracePeriodSeconds) (int $drain.timeoutSeconds)) -}}
{{- fail (printf "agent.drain.terminationGracePeriodSeconds (%v) must exceed agent.drain.timeoutSeconds (%v), or the drain is cut off mid-flush" $drain.terminationGracePeriodSeconds $drain.timeoutSeconds) -}}
{{- end -}}
{{- if not (gt (int $drain.timeoutSeconds) (int $drain.runnerGracePeriodSeconds)) -}}
{{- fail (printf "agent.drain.timeoutSeconds (%v) must exceed agent.drain.runnerGracePeriodSeconds (%v), or the drain cannot outlast the pods it deletes" $drain.timeoutSeconds $drain.runnerGracePeriodSeconds) -}}
{{- end -}}
{{- end }}

{{/*
Create the name of the service account to use
*/}}
{{- define "kubimo-controller.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "kubimo-controller.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}
